mod client;
mod error;
mod http_client;
mod http_transport;
mod prompt_cache;
mod providers;
mod sse;
mod types;

pub use client::{
    base_url_for_mode, oauth_token_is_expired, read_base_url, read_xai_base_url,
    resolve_saved_oauth_token, resolve_startup_auth_source, MessageStream, OAuthTokenSet,
    ProviderClient,
};
pub use error::ApiError;
pub use http_client::{
    build_http_client, build_http_client_or_default, build_http_client_with, ProxyConfig,
};
pub use http_transport::{request_id_from_headers, HttpTransport, RetryPolicy};
pub use prompt_cache::{
    CacheBreakEvent, PromptCache, PromptCacheConfig, PromptCachePaths, PromptCacheRecord,
    PromptCacheStats,
};
pub use providers::anthropic::{
    is_anthropic_api_key, is_claude_code_oauth_token, is_proxy_auth_token, AnthropicClient,
    AnthropicClient as ApiClient, AuthSource, DEFAULT_BASE_URL,
};
pub use providers::codex::{CodexClient, DEFAULT_CODEX_BASE_URL};
pub use providers::gemini::GeminiClient;
pub use providers::openai_compat::{
    build_chat_completion_request, flatten_tool_result_content, is_reasoning_model,
    model_rejects_is_error_field, model_requires_reasoning_content_in_history, translate_message,
    OpenAiCompatClient, OpenAiCompatConfig,
};
pub use providers::registry::{
    max_tokens_for_model, max_tokens_for_model_from_config, max_tokens_for_model_with_override,
    model_token_limit, model_token_limit_from_config, preflight_message_request, resolve_model,
    resolve_model_alias_from_config, resolve_provider_from_config, ApiFormat, Credential,
    ModelConfigEntry, ModelProviderMapping, ModelTokenLimit, ProviderConnectionConfig,
    ResolvedProvider, SudoCodeConfig,
};
pub use providers::{
    detect_provider_kind, model_family_identity_for, model_family_identity_for_kind, AuthMode,
    ProviderKind,
};
pub use sse::{parse_frame, SseParser};
pub use types::{
    CacheHints, ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStartEvent,
    ContentBlockStopEvent, ImageSource, InputContentBlock, InputMessage, MessageDelta,
    MessageDeltaEvent, MessageRequest, MessageResponse, MessageStartEvent, MessageStopEvent,
    OutputContentBlock, StreamEvent, ToolChoice, ToolDefinition, ToolResultContentBlock, Usage,
};

pub use telemetry::{
    AnalyticsEvent, AnthropicRequestProfile, ClientIdentity, JsonlTelemetrySink,
    MemoryTelemetrySink, SessionTraceRecord, SessionTracer, TelemetryEvent, TelemetrySink,
    DEFAULT_ANTHROPIC_VERSION,
};
