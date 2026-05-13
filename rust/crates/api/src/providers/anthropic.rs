use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use runtime::format_usd;
use runtime::{
    load_oauth_credentials, save_oauth_credentials, OAuthConfig, OAuthRefreshRequest,
    OAuthTokenExchangeRequest,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use telemetry::{AnalyticsEvent, AnthropicRequestProfile, ClientIdentity, SessionTracer};

use crate::error::ApiError;
use crate::http_transport::{request_id_from_headers, HttpTransport, RetryPolicy};
use crate::prompt_cache::{PromptCache, PromptCacheRecord, PromptCacheStats};

use super::registry::{self, model_token_limit};
use super::{anthropic_missing_credentials, Provider, ProviderFuture};
use crate::sse::SseParser;
use crate::types::{MessageDeltaEvent, MessageRequest, MessageResponse, StreamEvent, Usage};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const OAUTH_SYSTEM_PREFIX: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSource {
    None,
    ApiKey(String),
    BearerToken(String),
    ApiKeyAndBearer {
        api_key: String,
        bearer_token: String,
    },
}

impl AuthSource {
    pub fn from_env() -> Result<Self, ApiError> {
        if let Some(api_key) = read_env_non_empty("ANTHROPIC_API_KEY")? {
            return Ok(Self::ApiKey(api_key));
        }
        if let Some(token) = read_env_non_empty("PROXY_AUTH_TOKEN")? {
            return Ok(Self::BearerToken(token));
        }
        if let Some(token) = read_env_non_empty("CLAUDE_CODE_OAUTH_TOKEN")? {
            return Ok(Self::BearerToken(token));
        }
        Err(anthropic_missing_credentials())
    }

    #[must_use]
    pub fn api_key(&self) -> Option<&str> {
        match self {
            Self::ApiKey(api_key) | Self::ApiKeyAndBearer { api_key, .. } => Some(api_key),
            Self::None | Self::BearerToken(_) => None,
        }
    }

    #[must_use]
    pub fn bearer_token(&self) -> Option<&str> {
        match self {
            Self::BearerToken(token)
            | Self::ApiKeyAndBearer {
                bearer_token: token,
                ..
            } => Some(token),
            Self::None | Self::ApiKey(_) => None,
        }
    }

    #[must_use]
    pub fn masked_authorization_header(&self) -> &'static str {
        if self.bearer_token().is_some() {
            "Bearer [REDACTED]"
        } else {
            "<absent>"
        }
    }

    pub fn apply(&self, mut request_builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(api_key) = self.api_key() {
            request_builder = request_builder.header("x-api-key", api_key);
        }
        if let Some(token) = self.bearer_token() {
            request_builder = request_builder.bearer_auth(token);
        }
        request_builder
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OAuthTokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<u64>,
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl From<OAuthTokenSet> for AuthSource {
    fn from(value: OAuthTokenSet) -> Self {
        Self::BearerToken(value.access_token)
    }
}

#[derive(Debug, Clone)]
pub struct AnthropicClient {
    http: HttpTransport,
    auth: AuthSource,
    base_url: String,
    retry_policy: RetryPolicy,
    request_profile: AnthropicRequestProfile,
    prompt_cache: Option<PromptCache>,
    last_prompt_cache_record: Arc<Mutex<Option<PromptCacheRecord>>>,
    /// When true, prepend `OAUTH_SYSTEM_PREFIX` as the first system content
    /// block. Required for OAuth subscription tokens where the server gates
    /// access by checking the first system block for an exact match.
    oauth_system_prefix: bool,
}

impl AnthropicClient {
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: HttpTransport::new(),
            auth: AuthSource::ApiKey(api_key.into()),
            base_url: DEFAULT_BASE_URL.to_string(),
            retry_policy: RetryPolicy::DEFAULT,
            request_profile: AnthropicRequestProfile::default(),
            prompt_cache: None,
            last_prompt_cache_record: Arc::new(Mutex::new(None)),
            oauth_system_prefix: false,
        }
    }

    #[must_use]
    pub fn from_auth(auth: AuthSource) -> Self {
        Self::from_auth_with_mode(auth, None)
    }

    /// Build from an `AuthSource` and an optional explicit `AuthMode`. When
    /// the mode is `Some(Subscription)` the OAuth system prefix and beta
    /// header are always applied; when `None` the legacy env-sniffer
    /// (`is_claude_code_oauth_token`) decides.
    #[must_use]
    pub fn from_auth_with_mode(auth: AuthSource, mode: Option<super::AuthMode>) -> Self {
        let is_subscription = match mode {
            Some(super::AuthMode::Subscription) => true,
            Some(_) => false,
            None => is_claude_code_oauth_token(),
        };
        let mut client = Self {
            http: HttpTransport::new(),
            auth,
            base_url: DEFAULT_BASE_URL.to_string(),
            retry_policy: RetryPolicy::DEFAULT,
            request_profile: AnthropicRequestProfile::default(),
            prompt_cache: None,
            last_prompt_cache_record: Arc::new(Mutex::new(None)),
            oauth_system_prefix: is_subscription,
        };
        if is_subscription {
            // OAuth subscription tokens require the direct Anthropic API
            // and the oauth beta header.
            client.base_url = DEFAULT_BASE_URL.to_string();
            client.request_profile = client.request_profile.with_beta("oauth-2025-04-20");
        }
        client
    }

    pub fn from_env() -> Result<Self, ApiError> {
        Ok(Self::from_auth(AuthSource::from_env_or_saved()?).with_base_url(read_base_url()))
    }

    #[must_use]
    pub fn with_auth_source(mut self, auth: AuthSource) -> Self {
        self.auth = auth;
        self
    }

    #[must_use]
    pub fn with_auth_token(mut self, auth_token: Option<String>) -> Self {
        match (
            self.auth.api_key().map(ToOwned::to_owned),
            auth_token.filter(|token| !token.is_empty()),
        ) {
            (Some(api_key), Some(bearer_token)) => {
                self.auth = AuthSource::ApiKeyAndBearer {
                    api_key,
                    bearer_token,
                };
            }
            (Some(api_key), None) => {
                self.auth = AuthSource::ApiKey(api_key);
            }
            (None, Some(bearer_token)) => {
                self.auth = AuthSource::BearerToken(bearer_token);
            }
            (None, None) => {
                self.auth = AuthSource::None;
            }
        }
        self
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        // OAuth subscription tokens must always use the direct Anthropic API.
        if !self.oauth_system_prefix {
            self.base_url = base_url.into();
        }
        self
    }

    #[must_use]
    pub fn with_retry_policy(
        mut self,
        max_retries: u32,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Self {
        self.retry_policy = RetryPolicy {
            max_retries,
            initial_backoff,
            max_backoff,
        };
        self
    }

    #[must_use]
    pub fn with_session_tracer(mut self, session_tracer: SessionTracer) -> Self {
        self.http.set_session_tracer(session_tracer);
        self
    }

    #[must_use]
    pub fn with_client_identity(mut self, client_identity: ClientIdentity) -> Self {
        self.request_profile.client_identity = client_identity;
        self
    }

    #[must_use]
    pub fn with_beta(mut self, beta: impl Into<String>) -> Self {
        self.request_profile = self.request_profile.with_beta(beta);
        self
    }

    #[must_use]
    pub fn with_extra_body_param(mut self, key: impl Into<String>, value: Value) -> Self {
        self.request_profile = self.request_profile.with_extra_body(key, value);
        self
    }

    #[must_use]
    pub fn with_prompt_cache(mut self, prompt_cache: PromptCache) -> Self {
        self.prompt_cache = Some(prompt_cache);
        self
    }

    #[must_use]
    pub fn prompt_cache_stats(&self) -> Option<PromptCacheStats> {
        self.prompt_cache.as_ref().map(PromptCache::stats)
    }

    #[must_use]
    pub fn request_profile(&self) -> &AnthropicRequestProfile {
        &self.request_profile
    }

    #[must_use]
    pub fn session_tracer(&self) -> Option<&SessionTracer> {
        self.http.session_tracer()
    }

    #[must_use]
    pub fn prompt_cache(&self) -> Option<&PromptCache> {
        self.prompt_cache.as_ref()
    }

    #[must_use]
    pub fn take_last_prompt_cache_record(&self) -> Option<PromptCacheRecord> {
        self.last_prompt_cache_record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    #[must_use]
    pub fn with_request_profile(mut self, request_profile: AnthropicRequestProfile) -> Self {
        self.request_profile = request_profile;
        self
    }

    #[must_use]
    pub fn auth_source(&self) -> &AuthSource {
        &self.auth
    }

    pub async fn send_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageResponse, ApiError> {
        let request = MessageRequest {
            stream: false,
            ..request.clone()
        };

        if let Some(prompt_cache) = &self.prompt_cache {
            if let Some(response) = prompt_cache.lookup_completion(&request) {
                return Ok(response);
            }
        }

        self.preflight_message_request(&request).await?;

        let http_response = self.send_request(&request).await?;
        let request_id = request_id_from_headers(http_response.headers());
        let body = http_response.text().await.map_err(ApiError::from)?;
        let mut response = serde_json::from_str::<MessageResponse>(&body).map_err(|error| {
            ApiError::json_deserialize("Anthropic", &request.model, &body, error)
        })?;
        if response.request_id.is_none() {
            response.request_id = request_id;
        }

        if let Some(prompt_cache) = &self.prompt_cache {
            let record = prompt_cache.record_response(&request, &response);
            self.store_last_prompt_cache_record(record);
        }
        self.http.record_analytics(
            AnalyticsEvent::new("api", "message_usage")
                .with_property(
                    "request_id",
                    response
                        .request_id
                        .clone()
                        .map_or(Value::Null, Value::String),
                )
                .with_property("total_tokens", Value::from(response.total_tokens()))
                .with_property(
                    "estimated_cost_usd",
                    Value::String(format_usd(
                        response
                            .usage
                            .estimated_cost_usd(&response.model)
                            .total_cost_usd(),
                    )),
                ),
        );
        Ok(response)
    }

    pub async fn stream_message(
        &self,
        request: &MessageRequest,
    ) -> Result<MessageStream, ApiError> {
        self.preflight_message_request(request).await?;
        let response = self.send_request(&request.clone().with_streaming()).await?;
        Ok(MessageStream {
            request_id: request_id_from_headers(response.headers()),
            response,
            parser: SseParser::new().with_context("Anthropic", request.model.clone()),
            pending: VecDeque::new(),
            done: false,
            request: request.clone(),
            prompt_cache: self.prompt_cache.clone(),
            latest_usage: None,
            usage_recorded: false,
            last_prompt_cache_record: Arc::clone(&self.last_prompt_cache_record),
            session_tracer: self.session_tracer().cloned(),
        })
    }

    pub async fn exchange_oauth_code(
        &self,
        config: &OAuthConfig,
        request: &OAuthTokenExchangeRequest,
    ) -> Result<OAuthTokenSet, ApiError> {
        let response = self
            .http
            .raw()
            .post(&config.token_url)
            .header("content-type", "application/x-www-form-urlencoded")
            .form(&request.form_params())
            .send()
            .await
            .map_err(ApiError::from)?;
        let response = expect_success(response).await?;
        let body = response.text().await.map_err(ApiError::from)?;
        serde_json::from_str::<OAuthTokenSet>(&body).map_err(|error| {
            ApiError::json_deserialize("Anthropic OAuth (exchange)", "n/a", &body, error)
        })
    }

    pub async fn refresh_oauth_token(
        &self,
        config: &OAuthConfig,
        request: &OAuthRefreshRequest,
    ) -> Result<OAuthTokenSet, ApiError> {
        let response = self
            .http
            .raw()
            .post(&config.token_url)
            .header("content-type", "application/x-www-form-urlencoded")
            .form(&request.form_params())
            .send()
            .await
            .map_err(ApiError::from)?;
        let response = expect_success(response).await?;
        let body = response.text().await.map_err(ApiError::from)?;
        serde_json::from_str::<OAuthTokenSet>(&body).map_err(|error| {
            ApiError::json_deserialize("Anthropic OAuth (refresh)", "n/a", &body, error)
        })
    }

    /// Build URL, headers, body and send through `HttpTransport` with retries.
    async fn send_request(&self, request: &MessageRequest) -> Result<reqwest::Response, ApiError> {
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let mut body = self.request_profile.render_json_body(request)?;
        strip_unsupported_beta_body_fields(&mut body);
        apply_cache_hints(&mut body, request);
        self.prepend_oauth_system_prefix(&mut body);

        let mut headers: Vec<(String, String)> =
            vec![("content-type".to_string(), "application/json".to_string())];
        if let Some(api_key) = self.auth.api_key() {
            headers.push(("x-api-key".to_string(), api_key.to_string()));
        }
        if let Some(token) = self.auth.bearer_token() {
            headers.push(("authorization".to_string(), format!("Bearer {token}")));
        }
        headers.extend(self.request_profile.header_pairs());

        self.http
            .send_json(&url, &headers, &body, &self.retry_policy, expect_success)
            .await
            .map_err(|e| enrich_bearer_auth_error(e, &self.auth))
    }

    /// When `oauth_system_prefix` is set, transform the `system` field into
    /// an array of content blocks with the prefix as the first block. This is
    /// required for OAuth subscription tokens where the server gates access
    /// by checking the first system block for an exact match.
    fn prepend_oauth_system_prefix(&self, body: &mut Value) {
        if !self.oauth_system_prefix {
            return;
        }
        let prefix = OAUTH_SYSTEM_PREFIX;
        let Some(obj) = body.as_object_mut() else {
            return;
        };
        let prefix_block = serde_json::json!({"type": "text", "text": prefix});
        if let Some(system_val) = obj.remove("system") {
            match system_val {
                Value::String(s) => {
                    let mut blocks = vec![prefix_block];
                    if !s.is_empty() {
                        blocks.push(serde_json::json!({"type": "text", "text": s}));
                    }
                    obj.insert("system".to_string(), Value::Array(blocks));
                }
                Value::Array(mut arr) => {
                    // Already an array (e.g. from cache blocks) — prepend the prefix.
                    arr.insert(0, prefix_block);
                    obj.insert("system".to_string(), Value::Array(arr));
                }
                other => {
                    // Unknown format — put it back unchanged.
                    obj.insert("system".to_string(), other);
                }
            }
        } else {
            obj.insert("system".to_string(), Value::Array(vec![prefix_block]));
        }
    }

    async fn preflight_message_request(&self, request: &MessageRequest) -> Result<(), ApiError> {
        // Always run the local byte-estimate guard first. This catches
        // oversized requests even if the remote count_tokens endpoint is
        // unreachable, misconfigured, or unimplemented (e.g., third-party
        // Anthropic-compatible gateways). If byte estimation already flags
        // the request as oversized, reject immediately without a network
        // round trip.
        registry::preflight_message_request(request)?;

        let Some(limit) = model_token_limit(&request.model) else {
            return Ok(());
        };

        // Best-effort refinement using the Anthropic count_tokens endpoint.
        // On any failure (network, parse, auth), fall back to the local
        // byte-estimate result which already passed above.
        let Ok(counted_input_tokens) = self.count_tokens(request).await else {
            return Ok(());
        };
        let estimated_total_tokens = counted_input_tokens.saturating_add(request.max_tokens);
        if estimated_total_tokens > limit.context_window_tokens {
            return Err(ApiError::ContextWindowExceeded {
                model: request.model.clone(),
                estimated_input_tokens: counted_input_tokens,
                requested_output_tokens: request.max_tokens,
                estimated_total_tokens,
                context_window_tokens: limit.context_window_tokens,
            });
        }

        Ok(())
    }

    async fn count_tokens(&self, request: &MessageRequest) -> Result<u32, ApiError> {
        #[derive(serde::Deserialize)]
        struct CountTokensResponse {
            input_tokens: u32,
        }

        let request_url = format!(
            "{}/v1/messages/count_tokens",
            self.base_url.trim_end_matches('/')
        );
        let mut request_body = self.request_profile.render_json_body(request)?;
        strip_unsupported_beta_body_fields(&mut request_body);
        apply_cache_hints(&mut request_body, request);
        self.prepend_oauth_system_prefix(&mut request_body);
        let mut builder = self
            .http
            .raw()
            .post(&request_url)
            .header("content-type", "application/json");
        if let Some(api_key) = self.auth.api_key() {
            builder = builder.header("x-api-key", api_key);
        }
        if let Some(token) = self.auth.bearer_token() {
            builder = builder.bearer_auth(token);
        }
        for (header_name, header_value) in self.request_profile.header_pairs() {
            builder = builder.header(header_name, header_value);
        }
        let response = builder
            .json(&request_body)
            .send()
            .await
            .map_err(ApiError::from)?;

        let response = expect_success(response).await?;
        let body = response.text().await.map_err(ApiError::from)?;
        let parsed = serde_json::from_str::<CountTokensResponse>(&body).map_err(|error| {
            ApiError::json_deserialize("Anthropic count_tokens", &request.model, &body, error)
        })?;
        Ok(parsed.input_tokens)
    }

    fn store_last_prompt_cache_record(&self, record: PromptCacheRecord) {
        *self
            .last_prompt_cache_record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(record);
    }
}

impl AuthSource {
    pub fn from_env_or_saved() -> Result<Self, ApiError> {
        if let Some(api_key) = read_env_non_empty("ANTHROPIC_API_KEY")? {
            return Ok(Self::ApiKey(api_key));
        }
        if let Some(bearer_token) = read_env_non_empty("PROXY_AUTH_TOKEN")? {
            if read_env_non_empty("PROXY_BASE_URL")?.is_none() {
                return Err(ApiError::Auth(
                    "PROXY_AUTH_TOKEN requires PROXY_BASE_URL to be set.".to_string(),
                ));
            }
            return Ok(Self::BearerToken(bearer_token));
        }
        if let Some(bearer_token) = read_env_non_empty("CLAUDE_CODE_OAUTH_TOKEN")? {
            return Ok(Self::BearerToken(bearer_token));
        }
        if let Some(token_set) = load_saved_oauth_token()? {
            if !oauth_token_is_expired(&token_set) {
                return Ok(Self::BearerToken(token_set.access_token));
            }
        }
        Err(anthropic_missing_credentials())
    }

    /// Build an `AuthSource` for a specific `AuthMode`.
    pub fn for_mode(mode: super::AuthMode) -> Result<Self, ApiError> {
        match mode {
            super::AuthMode::Subscription => {
                let token = read_env_non_empty("CLAUDE_CODE_OAUTH_TOKEN")?.ok_or_else(|| {
                    ApiError::Auth(
                        "CLAUDE_CODE_OAUTH_TOKEN is required for subscription mode.".to_string(),
                    )
                })?;
                Ok(Self::BearerToken(token))
            }
            super::AuthMode::Proxy => {
                let token = read_env_non_empty("PROXY_AUTH_TOKEN")?.ok_or_else(|| {
                    ApiError::Auth("PROXY_AUTH_TOKEN is required for proxy mode.".to_string())
                })?;
                Ok(Self::BearerToken(token))
            }
            super::AuthMode::ApiKey => {
                // Try ANTHROPIC_API_KEY first.  When it is absent, return
                // AuthSource::None — non-Anthropic providers (xai, openai)
                // load their own keys via from_resolved, so the auth
                // source from here is unused in those paths.
                match read_env_non_empty("ANTHROPIC_API_KEY")? {
                    Some(api_key) => Ok(Self::ApiKey(api_key)),
                    None => Ok(Self::None),
                }
            }
        }
    }
}

/// Returns `true` when the active auth source comes from `CLAUDE_CODE_OAUTH_TOKEN`
/// and no higher-priority auth env var (`ANTHROPIC_API_KEY`) takes precedence.
#[must_use]
pub fn is_claude_code_oauth_token() -> bool {
    if read_env_non_empty("ANTHROPIC_API_KEY")
        .ok()
        .flatten()
        .is_some()
    {
        return false;
    }
    read_env_non_empty("CLAUDE_CODE_OAUTH_TOKEN")
        .ok()
        .flatten()
        .is_some()
}

/// Returns `true` when `ANTHROPIC_API_KEY` is set and non-empty.
#[must_use]
pub fn is_anthropic_api_key() -> bool {
    read_env_non_empty("ANTHROPIC_API_KEY")
        .ok()
        .flatten()
        .is_some()
}

/// Returns `true` when `PROXY_AUTH_TOKEN` is set and non-empty.
#[must_use]
pub fn is_proxy_auth_token() -> bool {
    read_env_non_empty("PROXY_AUTH_TOKEN")
        .ok()
        .flatten()
        .is_some()
}

#[must_use]
pub fn oauth_token_is_expired(token_set: &OAuthTokenSet) -> bool {
    token_set
        .expires_at
        .is_some_and(|expires_at| expires_at <= now_unix_timestamp())
}

pub fn resolve_saved_oauth_token(config: &OAuthConfig) -> Result<Option<OAuthTokenSet>, ApiError> {
    let Some(token_set) = load_saved_oauth_token()? else {
        return Ok(None);
    };
    resolve_saved_oauth_token_set(config, token_set).map(Some)
}

pub fn resolve_startup_auth_source<F>(load_oauth_config: F) -> Result<AuthSource, ApiError>
where
    F: FnOnce() -> Result<Option<OAuthConfig>, ApiError>,
{
    if let Some(api_key) = read_env_non_empty("ANTHROPIC_API_KEY")? {
        return Ok(AuthSource::ApiKey(api_key));
    }
    if let Some(bearer_token) = read_env_non_empty("PROXY_AUTH_TOKEN")? {
        return Ok(AuthSource::BearerToken(bearer_token));
    }
    if let Some(bearer_token) = read_env_non_empty("CLAUDE_CODE_OAUTH_TOKEN")? {
        return Ok(AuthSource::BearerToken(bearer_token));
    }
    if let Some(token_set) = load_saved_oauth_token()? {
        if let Some(config) = load_oauth_config()? {
            let resolved = resolve_saved_oauth_token_set(&config, token_set)?;
            return Ok(AuthSource::BearerToken(resolved.access_token));
        }
        if !oauth_token_is_expired(&token_set) {
            return Ok(AuthSource::BearerToken(token_set.access_token));
        }
    }
    Err(anthropic_missing_credentials())
}

fn resolve_saved_oauth_token_set(
    config: &OAuthConfig,
    token_set: OAuthTokenSet,
) -> Result<OAuthTokenSet, ApiError> {
    if !oauth_token_is_expired(&token_set) {
        return Ok(token_set);
    }
    let Some(refresh_token) = token_set.refresh_token.clone() else {
        return Err(ApiError::ExpiredOAuthToken);
    };
    let client = AnthropicClient::from_auth(AuthSource::None).with_base_url(read_base_url());
    let refreshed = client_runtime_block_on(async {
        client
            .refresh_oauth_token(
                config,
                &OAuthRefreshRequest::from_config(
                    config,
                    refresh_token,
                    Some(token_set.scopes.clone()),
                ),
            )
            .await
    })?;
    let resolved = OAuthTokenSet {
        access_token: refreshed.access_token,
        refresh_token: refreshed.refresh_token.or(token_set.refresh_token),
        expires_at: refreshed.expires_at,
        scopes: refreshed.scopes,
    };
    save_oauth_credentials(&runtime::OAuthTokenSet {
        access_token: resolved.access_token.clone(),
        refresh_token: resolved.refresh_token.clone(),
        expires_at: resolved.expires_at,
        scopes: resolved.scopes.clone(),
    })
    .map_err(ApiError::from)?;
    Ok(resolved)
}

fn client_runtime_block_on<F, T>(future: F) -> Result<T, ApiError>
where
    F: std::future::Future<Output = Result<T, ApiError>>,
{
    tokio::runtime::Runtime::new()
        .map_err(ApiError::from)?
        .block_on(future)
}

fn load_saved_oauth_token() -> Result<Option<OAuthTokenSet>, ApiError> {
    let token_set = load_oauth_credentials().map_err(ApiError::from)?;
    Ok(token_set.map(|token_set| OAuthTokenSet {
        access_token: token_set.access_token,
        refresh_token: token_set.refresh_token,
        expires_at: token_set.expires_at,
        scopes: token_set.scopes,
    }))
}

fn now_unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn read_env_non_empty(key: &str) -> Result<Option<String>, ApiError> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(super::dotenv_value(key)),
        Err(error) => Err(ApiError::from(error)),
    }
}

#[cfg(test)]
fn read_api_key() -> Result<String, ApiError> {
    let auth = AuthSource::from_env_or_saved()?;
    auth.api_key()
        .or_else(|| auth.bearer_token())
        .map(ToOwned::to_owned)
        .ok_or_else(anthropic_missing_credentials)
}

#[cfg(test)]
fn read_auth_token() -> Option<String> {
    read_env_non_empty("PROXY_AUTH_TOKEN")
        .ok()
        .and_then(std::convert::identity)
}

/// Read the base URL. For proxy mode uses `PROXY_BASE_URL`, otherwise
/// falls back to `ANTHROPIC_BASE_URL` (undocumented test override) or
/// the default Anthropic API endpoint.
#[must_use]
pub fn read_base_url() -> String {
    std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

/// Return the base URL appropriate for the given auth mode.
#[must_use]
pub fn base_url_for_mode(mode: super::AuthMode) -> String {
    match mode {
        super::AuthMode::Proxy => {
            std::env::var("PROXY_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
        }
        super::AuthMode::Subscription => DEFAULT_BASE_URL.to_string(),
        super::AuthMode::ApiKey => read_base_url(),
    }
}

impl Provider for AnthropicClient {
    type Stream = MessageStream;

    fn send_message<'a>(
        &'a self,
        request: &'a MessageRequest,
    ) -> ProviderFuture<'a, MessageResponse> {
        Box::pin(async move { self.send_message(request).await })
    }

    fn stream_message<'a>(
        &'a self,
        request: &'a MessageRequest,
    ) -> ProviderFuture<'a, Self::Stream> {
        Box::pin(async move { self.stream_message(request).await })
    }
}

#[derive(Debug)]
pub struct MessageStream {
    request_id: Option<String>,
    response: reqwest::Response,
    parser: SseParser,
    pending: VecDeque<StreamEvent>,
    done: bool,
    request: MessageRequest,
    prompt_cache: Option<PromptCache>,
    latest_usage: Option<Usage>,
    usage_recorded: bool,
    last_prompt_cache_record: Arc<Mutex<Option<PromptCacheRecord>>>,
    session_tracer: Option<SessionTracer>,
}

impl MessageStream {
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }

    pub async fn next_event(&mut self) -> Result<Option<StreamEvent>, ApiError> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                self.observe_event(&event);
                return Ok(Some(event));
            }

            if self.done {
                let remaining = self.parser.finish()?;
                self.pending.extend(remaining);
                if let Some(event) = self.pending.pop_front() {
                    return Ok(Some(event));
                }
                return Ok(None);
            }

            match self.response.chunk().await? {
                Some(chunk) => {
                    self.pending.extend(self.parser.push(&chunk)?);
                }
                None => {
                    self.done = true;
                }
            }
        }
    }

    fn observe_event(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::MessageDelta(MessageDeltaEvent { usage, .. }) => {
                self.latest_usage = Some(usage.clone());
            }
            StreamEvent::MessageStop(_) => {
                if !self.usage_recorded {
                    if let Some(usage) = self.latest_usage.as_ref() {
                        if let Some(prompt_cache) = &self.prompt_cache {
                            let record = prompt_cache.record_usage(&self.request, usage);
                            *self
                                .last_prompt_cache_record
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(record);
                        }
                        if let Some(tracer) = &self.session_tracer {
                            tracer.record_usage(
                                usage.input_tokens,
                                usage.output_tokens,
                                usage.cache_creation_input_tokens,
                                usage.cache_read_input_tokens,
                            );
                        }
                    }
                    self.usage_recorded = true;
                }
            }
            _ => {}
        }
    }
}

async fn expect_success(response: reqwest::Response) -> Result<reqwest::Response, ApiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let request_id = request_id_from_headers(response.headers());
    let body = response.text().await.unwrap_or_else(|_| String::new());
    let parsed_error = serde_json::from_str::<AnthropicErrorEnvelope>(&body).ok();
    let retryable = is_retryable_status(status);

    Err(ApiError::Api {
        status,
        error_type: parsed_error
            .as_ref()
            .map(|error| error.error.error_type.clone()),
        message: parsed_error
            .as_ref()
            .map(|error| error.error.message.clone()),
        request_id,
        body,
        retryable,
        suggested_action: None,
    })
}

const fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 429 | 500 | 502 | 503 | 504)
}

/// Anthropic API keys (`sk-ant-*`) are accepted over the `x-api-key` header
/// and rejected with HTTP 401 "Invalid bearer token" when sent as a Bearer
/// token via `PROXY_AUTH_TOKEN`. When we detect this exact shape, append a
/// hint to the error message that points the user at the one-line fix.
const SK_ANT_BEARER_HINT: &str = "sk-ant-* keys go in ANTHROPIC_API_KEY (x-api-key header), not PROXY_AUTH_TOKEN (Bearer header). Move your key to ANTHROPIC_API_KEY.";

fn enrich_bearer_auth_error(error: ApiError, auth: &AuthSource) -> ApiError {
    let ApiError::Api {
        status,
        error_type,
        message,
        request_id,
        body,
        retryable,
        suggested_action,
    } = error
    else {
        return error;
    };
    if status.as_u16() != 401 {
        return ApiError::Api {
            status,
            error_type,
            message,
            request_id,
            body,
            retryable,
            suggested_action,
        };
    }
    let Some(bearer_token) = auth.bearer_token() else {
        return ApiError::Api {
            status,
            error_type,
            message,
            request_id,
            body,
            retryable,
            suggested_action,
        };
    };
    if !bearer_token.starts_with("sk-ant-") {
        return ApiError::Api {
            status,
            error_type,
            message,
            request_id,
            body,
            retryable,
            suggested_action,
        };
    }
    // Only append the hint when the AuthSource is pure BearerToken. If both
    // api_key and bearer_token are present (`ApiKeyAndBearer`), the x-api-key
    // header is already being sent alongside the Bearer header and the 401
    // is coming from a different cause — adding the hint would be misleading.
    if auth.api_key().is_some() {
        return ApiError::Api {
            status,
            error_type,
            message,
            request_id,
            body,
            retryable,
            suggested_action,
        };
    }
    let enriched_message = match message {
        Some(existing) => Some(format!("{existing} — hint: {SK_ANT_BEARER_HINT}")),
        None => Some(format!("hint: {SK_ANT_BEARER_HINT}")),
    };
    ApiError::Api {
        status,
        error_type,
        message: enriched_message,
        request_id,
        body,
        retryable,
        suggested_action,
    }
}

/// Remove beta-only body fields that the standard `/v1/messages` and
/// `/v1/messages/count_tokens` endpoints reject as `Extra inputs are not
/// permitted`. The `betas` opt-in is communicated via the `anthropic-beta`
/// HTTP header on these endpoints, never as a JSON body field.
fn strip_unsupported_beta_body_fields(body: &mut Value) {
    if let Some(object) = body.as_object_mut() {
        object.remove("betas");
        // These fields are OpenAI-compatible only; Anthropic rejects them.
        object.remove("frequency_penalty");
        object.remove("presence_penalty");
        // Anthropic uses "stop_sequences" not "stop". Convert if present.
        if let Some(stop_val) = object.remove("stop") {
            if stop_val.as_array().is_some_and(|a| !a.is_empty()) {
                object.insert("stop_sequences".to_string(), stop_val);
            }
        }
        // Strip thought_signature from tool_use content blocks. The API
        // rejects this field when extended thinking is not enabled.
        strip_thought_signatures(object);
    }
}

/// Translate provider-agnostic [`CacheHints`] into Anthropic-specific
/// `cache_control` markers on the JSON body.
///
/// - `system_static` → system block with `cache_control: {type: "ephemeral", scope: "global"}`
/// - `system_dynamic` → system block with `cache_control: {type: "ephemeral"}`
/// - `breakpoint_last_message` → `cache_control: {type: "ephemeral"}` on the last
///   content block of the last message
fn apply_cache_hints(body: &mut Value, request: &MessageRequest) {
    let Some(hints) = &request.cache_hints else {
        return;
    };
    let Some(obj) = body.as_object_mut() else {
        return;
    };

    // --- System blocks ---
    let mut system_blocks: Vec<Value> = Vec::new();
    if let Some(text) = &hints.system_static {
        if !text.is_empty() {
            system_blocks.push(serde_json::json!({
                "type": "text",
                "text": text,
                "cache_control": { "type": "ephemeral", "scope": "global" },
            }));
        }
    }
    if let Some(text) = &hints.system_dynamic {
        if !text.is_empty() {
            system_blocks.push(serde_json::json!({
                "type": "text",
                "text": text,
                "cache_control": { "type": "ephemeral" },
            }));
        }
    }
    if !system_blocks.is_empty() {
        obj.insert("system".to_string(), Value::Array(system_blocks));
    }

    // --- Message breakpoint ---
    if hints.breakpoint_last_message {
        if let Some(Value::Array(messages)) = obj.get_mut("messages") {
            if let Some(last_msg) = messages.last_mut() {
                if let Some(Value::Array(content)) = last_msg.get_mut("content") {
                    if let Some(last_block) = content.last_mut() {
                        if let Some(block_obj) = last_block.as_object_mut() {
                            block_obj.insert(
                                "cache_control".to_string(),
                                serde_json::json!({ "type": "ephemeral" }),
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Walk `messages[].content[]` and remove `thought_signature` from any
/// `tool_use` content blocks. Without an explicit `thinking` configuration
/// in the request the Anthropic API treats the field as an extra input.
fn strip_thought_signatures(body: &mut Map<String, Value>) {
    if let Some(Value::Array(messages)) = body.get_mut("messages") {
        for msg in messages.iter_mut() {
            if let Some(Value::Array(content)) = msg.get_mut("content") {
                for block in content.iter_mut() {
                    if let Some(obj) = block.as_object_mut() {
                        if obj.get("type").and_then(Value::as_str) == Some("tool_use") {
                            obj.remove("thought_signature");
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorEnvelope {
    error: AnthropicErrorBody,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorBody {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

#[cfg(test)]
mod tests {
    use crate::http_transport::{request_id_from_headers, RetryPolicy};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use runtime::{clear_oauth_credentials, save_oauth_credentials, OAuthConfig};

    use super::{
        now_unix_timestamp, oauth_token_is_expired, resolve_saved_oauth_token,
        resolve_startup_auth_source, AnthropicClient, AuthSource, OAuthTokenSet,
    };
    use crate::types::{ContentBlockDelta, MessageRequest};

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn temp_config_home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "api-oauth-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ))
    }

    fn cleanup_temp_config_home(config_home: &std::path::Path) {
        match std::fs::remove_dir_all(config_home) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("cleanup temp dir: {error}"),
        }
    }

    fn sample_oauth_config(token_url: String) -> OAuthConfig {
        OAuthConfig {
            client_id: "runtime-client".to_string(),
            authorize_url: "https://console.test/oauth/authorize".to_string(),
            token_url,
            callback_port: Some(4545),
            manual_redirect_url: Some("https://console.test/oauth/callback".to_string()),
            scopes: vec!["org:read".to_string(), "user:write".to_string()],
        }
    }

    fn spawn_token_server(response_body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let address = listener.local_addr().expect("local addr");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).expect("read request");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        format!("http://{address}/oauth/token")
    }

    #[test]
    fn read_api_key_requires_presence() {
        let _guard = env_lock();
        std::env::remove_var("PROXY_AUTH_TOKEN");
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        let error = super::read_api_key().expect_err("missing key should error");
        assert!(matches!(
            error,
            crate::error::ApiError::MissingCredentials { .. }
        ));
    }

    #[test]
    fn read_api_key_requires_non_empty_value() {
        let _guard = env_lock();
        std::env::set_var("PROXY_AUTH_TOKEN", "");
        std::env::remove_var("ANTHROPIC_API_KEY");
        let error = super::read_api_key().expect_err("empty key should error");
        assert!(matches!(
            error,
            crate::error::ApiError::MissingCredentials { .. }
        ));
        std::env::remove_var("PROXY_AUTH_TOKEN");
    }

    #[test]
    fn read_api_key_prefers_api_key_env() {
        let _guard = env_lock();
        std::env::set_var("PROXY_AUTH_TOKEN", "auth-token");
        std::env::set_var("ANTHROPIC_API_KEY", "legacy-key");
        assert_eq!(
            super::read_api_key().expect("api key should load"),
            "legacy-key"
        );
        std::env::remove_var("PROXY_AUTH_TOKEN");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    fn read_auth_token_reads_auth_token_env() {
        let _guard = env_lock();
        std::env::set_var("PROXY_AUTH_TOKEN", "auth-token");
        assert_eq!(super::read_auth_token().as_deref(), Some("auth-token"));
        std::env::remove_var("PROXY_AUTH_TOKEN");
    }

    #[test]
    fn oauth_token_maps_to_bearer_auth_source() {
        let auth = AuthSource::from(OAuthTokenSet {
            access_token: "access-token".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: Some(123),
            scopes: vec!["scope:a".to_string()],
        });
        assert_eq!(auth.bearer_token(), Some("access-token"));
        assert_eq!(auth.api_key(), None);
    }

    #[test]
    fn auth_source_from_env_prefers_api_key_over_proxy_token() {
        let _guard = env_lock();
        std::env::set_var("PROXY_AUTH_TOKEN", "proxy-token");
        std::env::set_var("ANTHROPIC_API_KEY", "api-key");
        let auth = AuthSource::from_env().expect("env auth");
        assert_eq!(auth.api_key(), Some("api-key"));
        assert_eq!(auth.bearer_token(), None);
        std::env::remove_var("PROXY_AUTH_TOKEN");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    fn auth_source_from_env_or_saved_uses_saved_oauth_when_env_absent() {
        let _guard = env_lock();
        let config_home = temp_config_home();
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &config_home);
        std::env::remove_var("PROXY_AUTH_TOKEN");
        std::env::remove_var("ANTHROPIC_API_KEY");

        save_oauth_credentials(&runtime::OAuthTokenSet {
            access_token: "saved-access-token".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: Some(now_unix_timestamp() + 300),
            scopes: vec!["scope:a".to_string()],
        })
        .expect("save oauth credentials");

        let auth = AuthSource::from_env_or_saved().expect("saved oauth should be used");
        assert_eq!(auth.bearer_token(), Some("saved-access-token"));

        clear_oauth_credentials().expect("clear credentials");
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        cleanup_temp_config_home(&config_home);
    }

    #[test]
    fn oauth_token_expiry_uses_expires_at_timestamp() {
        assert!(oauth_token_is_expired(&OAuthTokenSet {
            access_token: "access-token".to_string(),
            refresh_token: None,
            expires_at: Some(1),
            scopes: Vec::new(),
        }));
        assert!(!oauth_token_is_expired(&OAuthTokenSet {
            access_token: "access-token".to_string(),
            refresh_token: None,
            expires_at: Some(now_unix_timestamp() + 60),
            scopes: Vec::new(),
        }));
    }

    #[test]
    fn resolve_saved_oauth_token_refreshes_expired_credentials() {
        let _guard = env_lock();
        let config_home = temp_config_home();
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &config_home);
        std::env::remove_var("PROXY_AUTH_TOKEN");
        std::env::remove_var("ANTHROPIC_API_KEY");
        save_oauth_credentials(&runtime::OAuthTokenSet {
            access_token: "expired-access-token".to_string(),
            refresh_token: Some("refresh-token".to_string()),
            expires_at: Some(1),
            scopes: vec!["scope:a".to_string()],
        })
        .expect("save expired oauth credentials");

        let token_url = spawn_token_server(
            "{\"access_token\":\"refreshed-token\",\"refresh_token\":\"fresh-refresh\",\"expires_at\":9999999999,\"scopes\":[\"scope:a\"]}",
        );
        let resolved = resolve_saved_oauth_token(&sample_oauth_config(token_url))
            .expect("resolve refreshed token")
            .expect("token set present");
        assert_eq!(resolved.access_token, "refreshed-token");
        let stored = runtime::load_oauth_credentials()
            .expect("load stored credentials")
            .expect("stored token set");
        assert_eq!(stored.access_token, "refreshed-token");

        clear_oauth_credentials().expect("clear credentials");
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        cleanup_temp_config_home(&config_home);
    }

    #[test]
    fn resolve_startup_auth_source_uses_saved_oauth_when_no_env() {
        let _guard = env_lock();
        let config_home = temp_config_home();
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &config_home);
        std::env::remove_var("PROXY_AUTH_TOKEN");
        std::env::remove_var("ANTHROPIC_API_KEY");

        save_oauth_credentials(&runtime::OAuthTokenSet {
            access_token: "saved-access-token".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: Some(now_unix_timestamp() + 300),
            scopes: vec!["scope:a".to_string()],
        })
        .expect("save oauth credentials");

        let auth = resolve_startup_auth_source(|| Ok(None)).expect("saved oauth should be used");
        assert_eq!(auth.bearer_token(), Some("saved-access-token"));

        clear_oauth_credentials().expect("clear credentials");
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        cleanup_temp_config_home(&config_home);
    }

    #[test]
    fn resolve_saved_oauth_token_preserves_refresh_token_when_refresh_response_omits_it() {
        let _guard = env_lock();
        let config_home = temp_config_home();
        std::env::set_var("SUDO_CODE_CONFIG_HOME", &config_home);
        std::env::remove_var("PROXY_AUTH_TOKEN");
        std::env::remove_var("ANTHROPIC_API_KEY");
        save_oauth_credentials(&runtime::OAuthTokenSet {
            access_token: "expired-access-token".to_string(),
            refresh_token: Some("refresh-token".to_string()),
            expires_at: Some(1),
            scopes: vec!["scope:a".to_string()],
        })
        .expect("save expired oauth credentials");

        let token_url = spawn_token_server(
            "{\"access_token\":\"refreshed-token\",\"expires_at\":9999999999,\"scopes\":[\"scope:a\"]}",
        );
        let resolved = resolve_saved_oauth_token(&sample_oauth_config(token_url))
            .expect("resolve refreshed token")
            .expect("token set present");
        assert_eq!(resolved.access_token, "refreshed-token");
        assert_eq!(resolved.refresh_token.as_deref(), Some("refresh-token"));
        let stored = runtime::load_oauth_credentials()
            .expect("load stored credentials")
            .expect("stored token set");
        assert_eq!(stored.refresh_token.as_deref(), Some("refresh-token"));

        clear_oauth_credentials().expect("clear credentials");
        std::env::remove_var("SUDO_CODE_CONFIG_HOME");
        cleanup_temp_config_home(&config_home);
    }

    #[test]
    fn message_request_stream_helper_sets_stream_true() {
        let request = MessageRequest {
            model: "claude-opus-4-6".to_string(),
            max_tokens: 64,
            messages: vec![],
            system: None,
            tools: None,
            tool_choice: None,
            stream: false,
            ..Default::default()
        };

        assert!(request.with_streaming().stream);
    }

    #[test]
    fn backoff_doubles_until_maximum() {
        let policy = RetryPolicy {
            max_retries: 3,
            initial_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(25),
        };
        // Verify with_retry_policy wires through correctly.
        let client = AnthropicClient::new("test-key").with_retry_policy(
            policy.max_retries,
            policy.initial_backoff,
            policy.max_backoff,
        );
        assert_eq!(client.retry_policy.max_retries, 3);
        assert_eq!(
            client.retry_policy.initial_backoff,
            Duration::from_millis(10)
        );
        assert_eq!(client.retry_policy.max_backoff, Duration::from_millis(25));
    }

    #[test]
    fn retryable_statuses_are_detected() {
        assert!(super::is_retryable_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(super::is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(!super::is_retryable_status(
            reqwest::StatusCode::UNAUTHORIZED
        ));
    }

    #[test]
    fn tool_delta_variant_round_trips() {
        let delta = ContentBlockDelta::InputJsonDelta {
            partial_json: "{\"city\":\"Paris\"}".to_string(),
        };
        let encoded = serde_json::to_string(&delta).expect("delta should serialize");
        let decoded: ContentBlockDelta =
            serde_json::from_str(&encoded).expect("delta should deserialize");
        assert_eq!(decoded, delta);
    }

    #[test]
    fn request_id_uses_primary_or_fallback_header() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("request-id", "req_primary".parse().expect("header"));
        assert_eq!(
            request_id_from_headers(&headers).as_deref(),
            Some("req_primary")
        );

        headers.clear();
        headers.insert("x-request-id", "req_fallback".parse().expect("header"));
        assert_eq!(
            request_id_from_headers(&headers).as_deref(),
            Some("req_fallback")
        );
    }

    #[test]
    fn auth_source_applies_headers() {
        let auth = AuthSource::ApiKeyAndBearer {
            api_key: "test-key".to_string(),
            bearer_token: "proxy-token".to_string(),
        };
        let request = auth
            .apply(reqwest::Client::new().post("https://example.test"))
            .build()
            .expect("request build");
        let headers = request.headers();
        assert_eq!(
            headers.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("test-key")
        );
        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer proxy-token")
        );
    }

    #[test]
    fn strip_unsupported_beta_body_fields_removes_betas_array() {
        let mut body = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "betas": ["claude-code-20250219", "prompt-caching-scope-2026-01-05"],
            "metadata": {"source": "test"},
        });

        super::strip_unsupported_beta_body_fields(&mut body);

        assert!(
            body.get("betas").is_none(),
            "betas body field must be stripped before sending to /v1/messages"
        );
        assert_eq!(
            body.get("model").and_then(serde_json::Value::as_str),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(body["max_tokens"], serde_json::json!(1024));
        assert_eq!(body["metadata"]["source"], serde_json::json!("test"));
    }

    #[test]
    fn strip_unsupported_beta_body_fields_is_a_noop_when_betas_absent() {
        let mut body = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
        });
        let original = body.clone();

        super::strip_unsupported_beta_body_fields(&mut body);

        assert_eq!(body, original);
    }

    #[test]
    fn strip_removes_openai_only_fields_and_converts_stop() {
        let mut body = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "temperature": 0.7,
            "frequency_penalty": 0.5,
            "presence_penalty": 0.3,
            "stop": ["\n"],
        });

        super::strip_unsupported_beta_body_fields(&mut body);

        // temperature is kept (Anthropic supports it)
        assert_eq!(body["temperature"], serde_json::json!(0.7));
        // frequency_penalty and presence_penalty are removed
        assert!(
            body.get("frequency_penalty").is_none(),
            "frequency_penalty must be stripped for Anthropic"
        );
        assert!(
            body.get("presence_penalty").is_none(),
            "presence_penalty must be stripped for Anthropic"
        );
        // stop is renamed to stop_sequences
        assert!(body.get("stop").is_none(), "stop must be renamed");
        assert_eq!(body["stop_sequences"], serde_json::json!(["\n"]));
    }

    #[test]
    fn strip_does_not_add_empty_stop_sequences() {
        let mut body = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "max_tokens": 1024,
            "stop": [],
        });

        super::strip_unsupported_beta_body_fields(&mut body);

        assert!(body.get("stop").is_none());
        assert!(
            body.get("stop_sequences").is_none(),
            "empty stop should not produce stop_sequences"
        );
    }

    #[test]
    fn rendered_request_body_strips_betas_for_standard_messages_endpoint() {
        let client = AnthropicClient::new("test-key").with_beta("tools-2026-04-01");
        let request = MessageRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 64,
            messages: vec![],
            system: None,
            tools: None,
            tool_choice: None,
            stream: false,
            ..Default::default()
        };

        let mut rendered = client
            .request_profile()
            .render_json_body(&request)
            .expect("body should render");
        assert!(
            rendered.get("betas").is_some(),
            "render_json_body still emits betas; the strip helper guards the wire format",
        );
        super::strip_unsupported_beta_body_fields(&mut rendered);

        assert!(
            rendered.get("betas").is_none(),
            "betas must not appear in /v1/messages request bodies"
        );
        assert_eq!(
            rendered.get("model").and_then(serde_json::Value::as_str),
            Some("claude-sonnet-4-6")
        );
    }

    #[test]
    fn enrich_bearer_auth_error_appends_sk_ant_hint_on_401_with_pure_bearer_token() {
        // given
        let auth = AuthSource::BearerToken("sk-ant-api03-deadbeef".to_string());
        let error = crate::error::ApiError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            error_type: Some("authentication_error".to_string()),
            message: Some("Invalid bearer token".to_string()),
            request_id: Some("req_varleg_001".to_string()),
            body: String::new(),
            retryable: false,
            suggested_action: None,
        };

        // when
        let enriched = super::enrich_bearer_auth_error(error, &auth);

        // then
        let rendered = enriched.to_string();
        assert!(
            rendered.contains("Invalid bearer token"),
            "existing provider message should be preserved: {rendered}"
        );
        assert!(
            rendered.contains(
                "sk-ant-* keys go in ANTHROPIC_API_KEY (x-api-key header), not PROXY_AUTH_TOKEN (Bearer header). Move your key to ANTHROPIC_API_KEY."
            ),
            "rendered error should include the sk-ant-* hint: {rendered}"
        );
        assert!(
            rendered.contains("[trace req_varleg_001]"),
            "request id should still flow through the enriched error: {rendered}"
        );
        match enriched {
            crate::error::ApiError::Api { status, .. } => {
                assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
            }
            other => panic!("expected Api variant, got {other:?}"),
        }
    }

    #[test]
    fn enrich_bearer_auth_error_leaves_non_401_errors_unchanged() {
        // given
        let auth = AuthSource::BearerToken("sk-ant-api03-deadbeef".to_string());
        let error = crate::error::ApiError::Api {
            status: reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            error_type: Some("api_error".to_string()),
            message: Some("internal server error".to_string()),
            request_id: None,
            body: String::new(),
            retryable: true,
            suggested_action: None,
        };

        // when
        let enriched = super::enrich_bearer_auth_error(error, &auth);

        // then
        let rendered = enriched.to_string();
        assert!(
            !rendered.contains("sk-ant-*"),
            "non-401 errors must not be annotated with the bearer hint: {rendered}"
        );
        assert!(
            rendered.contains("internal server error"),
            "original message must be preserved verbatim: {rendered}"
        );
    }

    #[test]
    fn enrich_bearer_auth_error_ignores_401_when_bearer_token_is_not_sk_ant() {
        // given
        let auth = AuthSource::BearerToken("oauth-access-token-opaque".to_string());
        let error = crate::error::ApiError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            error_type: Some("authentication_error".to_string()),
            message: Some("Invalid bearer token".to_string()),
            request_id: None,
            body: String::new(),
            retryable: false,
            suggested_action: None,
        };

        // when
        let enriched = super::enrich_bearer_auth_error(error, &auth);

        // then
        let rendered = enriched.to_string();
        assert!(
            !rendered.contains("sk-ant-*"),
            "oauth-style bearer tokens must not trigger the sk-ant-* hint: {rendered}"
        );
    }

    #[test]
    fn enrich_bearer_auth_error_skips_hint_when_api_key_header_is_also_present() {
        // given
        let auth = AuthSource::ApiKeyAndBearer {
            api_key: "sk-ant-api03-legitimate".to_string(),
            bearer_token: "sk-ant-api03-deadbeef".to_string(),
        };
        let error = crate::error::ApiError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            error_type: Some("authentication_error".to_string()),
            message: Some("Invalid bearer token".to_string()),
            request_id: None,
            body: String::new(),
            retryable: false,
            suggested_action: None,
        };

        // when
        let enriched = super::enrich_bearer_auth_error(error, &auth);

        // then
        let rendered = enriched.to_string();
        assert!(
            !rendered.contains("sk-ant-*"),
            "hint should be suppressed when x-api-key header is already being sent: {rendered}"
        );
    }

    #[test]
    fn enrich_bearer_auth_error_ignores_401_when_auth_source_has_no_bearer() {
        // given
        let auth = AuthSource::ApiKey("sk-ant-api03-legitimate".to_string());
        let error = crate::error::ApiError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            error_type: Some("authentication_error".to_string()),
            message: Some("Invalid x-api-key".to_string()),
            request_id: None,
            body: String::new(),
            retryable: false,
            suggested_action: None,
        };

        // when
        let enriched = super::enrich_bearer_auth_error(error, &auth);

        // then
        let rendered = enriched.to_string();
        assert!(
            !rendered.contains("sk-ant-*"),
            "bearer hint must not apply when AuthSource is ApiKey-only: {rendered}"
        );
    }

    #[test]
    fn enrich_bearer_auth_error_passes_non_api_errors_through_unchanged() {
        // given
        let auth = AuthSource::BearerToken("sk-ant-api03-deadbeef".to_string());
        let error = crate::error::ApiError::InvalidSseFrame("unterminated event");

        // when
        let enriched = super::enrich_bearer_auth_error(error, &auth);

        // then
        assert!(matches!(
            enriched,
            crate::error::ApiError::InvalidSseFrame(_)
        ));
    }

    #[test]
    fn apply_cache_hints_produces_system_blocks_and_message_breakpoint() {
        use crate::types::{CacheHints, InputMessage};
        use telemetry::AnthropicRequestProfile;

        let request = MessageRequest {
            model: "claude-sonnet-4-6".to_string(),
            max_tokens: 1024,
            messages: vec![
                InputMessage::user_text("first question"),
                InputMessage::user_text("second question"),
            ],
            system: Some("flat fallback".to_string()),
            stream: true,
            cache_hints: Some(CacheHints {
                system_static: Some("static core instructions".to_string()),
                system_dynamic: Some("dynamic session context".to_string()),
                breakpoint_last_message: true,
            }),
            ..Default::default()
        };

        let mut body = AnthropicRequestProfile::default()
            .render_json_body(&request)
            .expect("render body");
        super::apply_cache_hints(&mut body, &request);

        // --- System blocks ---
        let system = body.get("system").expect("system field should exist");
        let sys_blocks = system.as_array().expect("system should be an array");
        assert_eq!(sys_blocks.len(), 2);

        assert_eq!(sys_blocks[0]["text"], "static core instructions");
        assert_eq!(sys_blocks[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(sys_blocks[0]["cache_control"]["scope"], "global");

        assert_eq!(sys_blocks[1]["text"], "dynamic session context");
        assert_eq!(sys_blocks[1]["cache_control"]["type"], "ephemeral");
        assert!(sys_blocks[1]["cache_control"].get("scope").is_none());

        // --- Message breakpoint on last message ---
        let messages = body["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        // First message: no cache_control.
        assert!(messages[0]["content"][0].get("cache_control").is_none());
        // Last message's last content block: has cache_control.
        let last_block = &messages[1]["content"][0];
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");
    }
}
