use std::ffi::OsString;
use std::sync::{Mutex, OnceLock};

use api::{
    read_xai_base_url, ApiFormat, Credential, ProviderClient, ProviderKind, ResolvedProvider,
};

#[test]
fn provider_client_routes_xai_through_from_resolved() {
    let resolved = ResolvedProvider {
        kind: ProviderKind::Xai,
        api_format: ApiFormat::OpenAiCompletions,
        base_url: "https://api.x.ai/v1".to_string(),
        credential: Credential::ApiKey("xai-test-key".to_string()),
        model_id: "grok-3-mini".to_string(),
    };

    let client = ProviderClient::from_resolved(&resolved, None)
        .expect("xai resolved provider should construct");

    assert_eq!(client.provider_kind(), ProviderKind::Xai);
}

#[test]
fn provider_client_routes_anthropic_through_from_resolved() {
    let resolved = ResolvedProvider {
        kind: ProviderKind::Anthropic,
        api_format: ApiFormat::AnthropicMessages,
        base_url: "https://api.anthropic.com".to_string(),
        credential: Credential::ApiKey("anthropic-test-key".to_string()),
        model_id: "claude-sonnet-4-6".to_string(),
    };

    let client = ProviderClient::from_resolved(&resolved, None)
        .expect("anthropic resolved provider should construct");

    assert_eq!(client.provider_kind(), ProviderKind::Anthropic);
}

#[test]
fn read_xai_base_url_prefers_env_override() {
    let _lock = env_lock();
    let _xai_base_url = EnvVarGuard::set("XAI_BASE_URL", Some("https://example.xai.test/v1"));

    assert_eq!(read_xai_base_url(), "https://example.xai.test/v1");
}

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct EnvVarGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: Option<&str>) -> Self {
        let original = std::env::var_os(key);
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}
