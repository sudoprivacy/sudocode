#![allow(clippy::cast_possible_truncation)]
use std::future::Future;
use std::pin::Pin;

use crate::error::ApiError;
use crate::types::{MessageRequest, MessageResponse};

pub mod anthropic;
pub mod codex;
pub mod gemini;
pub mod openai_compat;
pub mod registry;

/// Explicit auth mode selected via `--auth` CLI flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// OAuth subscription via `CLAUDE_CODE_OAUTH_TOKEN`.
    Subscription,
    /// Proxy bearer token via `PROXY_AUTH_TOKEN` + `PROXY_BASE_URL`.
    Proxy,
    /// Direct API key (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.).
    ApiKey,
}

impl AuthMode {
    /// Parse from a CLI string value.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "subscription" => Ok(Self::Subscription),
            "proxy" => Ok(Self::Proxy),
            "api-key" => Ok(Self::ApiKey),
            other => Err(format!(
                "invalid value for --auth: '{other}'; must be subscription, proxy, or api-key"
            )),
        }
    }

    /// Round-trip-safe string for parsing and display (matches `parse()` input).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Subscription => "subscription",
            Self::Proxy => "proxy",
            Self::ApiKey => "api-key",
        }
    }

    /// Human-readable label for display in the connected line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Subscription => "subscription",
            Self::Proxy => "proxy",
            Self::ApiKey => "api key",
        }
    }
}

#[allow(dead_code)]
pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ApiError>> + Send + 'a>>;

#[allow(dead_code)]
pub trait Provider {
    type Stream;

    fn send_message<'a>(
        &'a self,
        request: &'a MessageRequest,
    ) -> ProviderFuture<'a, MessageResponse>;

    fn stream_message<'a>(
        &'a self,
        request: &'a MessageRequest,
    ) -> ProviderFuture<'a, Self::Stream>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    Xai,
    OpenAi,
    Codex,
    Gemini,
}

/// Best-effort heuristic to infer the provider kind from a model name
/// (e.g. `openai/gpt-4.1-mini` → `OpenAi`, `grok-3` → `Xai`).
///
/// Used only for prompt identity resolution; the full config-based
/// resolution in the registry takes precedence for actual API routing.
#[must_use]
pub fn detect_provider_kind(model: &str) -> ProviderKind {
    let lowered = model.to_ascii_lowercase();
    if let Some(prefix) = lowered.split('/').next() {
        match prefix {
            "openai" => return ProviderKind::OpenAi,
            "gemini" => return ProviderKind::Gemini,
            "codex" => return ProviderKind::Codex,
            "xai" | "grok" => return ProviderKind::Xai,
            _ => {}
        }
    }
    let canonical = lowered.rsplit('/').next().unwrap_or(lowered.as_str());
    if canonical.starts_with("grok") {
        return ProviderKind::Xai;
    }
    if canonical.starts_with("gpt-")
        || canonical.starts_with("o1")
        || canonical.starts_with("o3")
        || canonical.starts_with("o4")
        || canonical.starts_with("deepseek")
        || canonical.starts_with("qwen")
        || canonical.starts_with("kimi")
    {
        return ProviderKind::OpenAi;
    }
    if canonical.starts_with("gemini") {
        return ProviderKind::Gemini;
    }
    ProviderKind::Anthropic
}

#[must_use]
pub const fn model_family_identity_for_kind(kind: ProviderKind) -> runtime::ModelFamilyIdentity {
    match kind {
        ProviderKind::Anthropic => runtime::ModelFamilyIdentity::Claude,
        ProviderKind::Xai | ProviderKind::OpenAi | ProviderKind::Codex | ProviderKind::Gemini => {
            runtime::ModelFamilyIdentity::Generic
        }
    }
}

#[must_use]
pub fn model_family_identity_for(model: &str) -> runtime::ModelFamilyIdentity {
    model_family_identity_for_kind(detect_provider_kind(model))
}

/// Env var names used by other provider backends. When Anthropic auth
/// resolution fails we sniff these so we can hint the user that their
/// credentials probably belong to a different provider and suggest the
/// model-prefix routing fix that would select it.
const FOREIGN_PROVIDER_ENV_VARS: &[(&str, &str, &str)] = &[
    (
        "OPENAI_API_KEY",
        "OpenAI-compat",
        "prefix your model name with `openai/` (e.g. `--model openai/gpt-4.1-mini`) so prefix routing selects the OpenAI-compatible provider, and set `OPENAI_BASE_URL` if you are pointing at OpenRouter/Ollama/a local server",
    ),
    (
        "XAI_API_KEY",
        "xAI",
        "use an xAI model alias (e.g. `--model grok` or `--model grok-mini`) so the prefix router selects the xAI backend",
    ),
    (
        "DASHSCOPE_API_KEY",
        "Alibaba DashScope",
        "prefix your model name with `qwen/` or `qwen-` (e.g. `--model qwen-plus`) so prefix routing selects the DashScope backend",
    ),
];

/// Check whether an env var is set to a non-empty value either in the real
/// process environment or in the working-directory `.env` file. Mirrors the
/// credential discovery path used by `read_env_non_empty` so the hint text
/// stays truthful when users rely on `.env` instead of a real export.
fn env_or_dotenv_present(key: &str) -> bool {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => true,
        Ok(_) | Err(std::env::VarError::NotPresent) => {
            dotenv_value(key).is_some_and(|value| !value.is_empty())
        }
        Err(_) => false,
    }
}

/// Produce a hint string describing the first foreign provider credential
/// that is present in the environment when Anthropic auth resolution has
/// just failed. Returns `None` when no foreign credential is set, in which
/// case the caller should fall back to the plain `missing_credentials`
/// error without a hint.
pub(crate) fn anthropic_missing_credentials_hint() -> Option<String> {
    for (env_var, provider_label, fix_hint) in FOREIGN_PROVIDER_ENV_VARS {
        if env_or_dotenv_present(env_var) {
            return Some(format!(
                "I see {env_var} is set — if you meant to use the {provider_label} provider, {fix_hint}."
            ));
        }
    }
    None
}

/// Build an Anthropic-specific `MissingCredentials` error, attaching a
/// hint suggesting the probable fix whenever a different provider's
/// credentials are already present in the environment. Anthropic call
/// sites should prefer this helper over `ApiError::missing_credentials`
/// so users who mistyped a model name or forgot the prefix get a useful
/// signal instead of a generic "missing Anthropic credentials" wall.
pub(crate) fn anthropic_missing_credentials() -> ApiError {
    const PROVIDER: &str = "Anthropic";
    const ENV_VARS: &[&str] = &[
        "ANTHROPIC_API_KEY",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "PROXY_AUTH_TOKEN",
    ];
    match anthropic_missing_credentials_hint() {
        Some(hint) => ApiError::missing_credentials_with_hint(PROVIDER, ENV_VARS, hint),
        None => ApiError::missing_credentials(PROVIDER, ENV_VARS),
    }
}

/// Parse a `.env` file body into key/value pairs using a minimal `KEY=VALUE`
/// grammar. Lines that are blank, start with `#`, or do not contain `=` are
/// ignored. Surrounding double or single quotes are stripped from the value.
/// An optional leading `export ` prefix on the key is also stripped so files
/// shared with shell `source` workflows still parse cleanly.
pub(crate) fn parse_dotenv(content: &str) -> std::collections::HashMap<String, String> {
    let mut values = std::collections::HashMap::new();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((raw_key, raw_value)) = line.split_once('=') else {
            continue;
        };
        let trimmed_key = raw_key.trim();
        let key = trimmed_key
            .strip_prefix("export ")
            .map_or(trimmed_key, str::trim)
            .to_string();
        if key.is_empty() {
            continue;
        }
        let trimmed_value = raw_value.trim();
        let unquoted = if (trimmed_value.starts_with('"') && trimmed_value.ends_with('"')
            || trimmed_value.starts_with('\'') && trimmed_value.ends_with('\''))
            && trimmed_value.len() >= 2
        {
            &trimmed_value[1..trimmed_value.len() - 1]
        } else {
            trimmed_value
        };
        values.insert(key, unquoted.to_string());
    }
    values
}

/// Load and parse a `.env` file from the given path. Missing files yield
/// `None` instead of an error so callers can use this as a soft fallback.
pub(crate) fn load_dotenv_file(
    path: &std::path::Path,
) -> Option<std::collections::HashMap<String, String>> {
    let content = std::fs::read_to_string(path).ok()?;
    Some(parse_dotenv(&content))
}

/// Look up `key` in a `.env` file located in the current working directory.
/// Returns `None` when the file is missing, the key is absent, or the value
/// is empty.
pub(crate) fn dotenv_value(key: &str) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let values = load_dotenv_file(&cwd.join(".env"))?;
    values.get(key).filter(|value| !value.is_empty()).cloned()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::{Mutex, OnceLock};

    use crate::error::ApiError;

    use super::{
        anthropic_missing_credentials, anthropic_missing_credentials_hint, detect_provider_kind,
        load_dotenv_file, model_family_identity_for, model_family_identity_for_kind, parse_dotenv,
        ProviderKind,
    };

    /// Serializes every test in this module that mutates process-wide
    /// environment variables so concurrent test threads cannot observe
    /// each other's partially-applied state while probing the foreign
    /// provider credential sniffer.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Snapshot-restore guard for a single environment variable. Captures
    /// the original value on construction, applies the requested override
    /// (set or remove), and restores the original on drop so tests leave
    /// the process env untouched even when they panic mid-assertion.
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
            match self.original.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn parse_dotenv_extracts_keys_handles_comments_quotes_and_export_prefix() {
        // given
        let body = "\
# this is a comment

ANTHROPIC_API_KEY=plain-value
XAI_API_KEY=\"quoted-value\"
OPENAI_API_KEY='single-quoted'
export GROK_API_KEY=exported-value
   PADDED_KEY  =  padded-value
EMPTY_VALUE=
NO_EQUALS_LINE
";

        // when
        let values = parse_dotenv(body);

        // then
        assert_eq!(
            values.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("plain-value")
        );
        assert_eq!(
            values.get("XAI_API_KEY").map(String::as_str),
            Some("quoted-value")
        );
        assert_eq!(
            values.get("OPENAI_API_KEY").map(String::as_str),
            Some("single-quoted")
        );
        assert_eq!(
            values.get("GROK_API_KEY").map(String::as_str),
            Some("exported-value")
        );
        assert_eq!(
            values.get("PADDED_KEY").map(String::as_str),
            Some("padded-value")
        );
        assert_eq!(values.get("EMPTY_VALUE").map(String::as_str), Some(""));
        assert!(!values.contains_key("NO_EQUALS_LINE"));
        assert!(!values.contains_key("# this is a comment"));
    }

    #[test]
    fn load_dotenv_file_reads_keys_from_disk_and_returns_none_when_missing() {
        // given
        let temp_root = std::env::temp_dir().join(format!(
            "api-dotenv-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos())
        ));
        std::fs::create_dir_all(&temp_root).expect("create temp dir");
        let env_path = temp_root.join(".env");
        std::fs::write(
            &env_path,
            "ANTHROPIC_API_KEY=secret-from-file\n# comment\nXAI_API_KEY=\"xai-secret\"\n",
        )
        .expect("write .env");
        let missing_path = temp_root.join("does-not-exist.env");

        // when
        let loaded = load_dotenv_file(&env_path).expect("file should load");
        let missing = load_dotenv_file(&missing_path);

        // then
        assert_eq!(
            loaded.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("secret-from-file")
        );
        assert_eq!(
            loaded.get("XAI_API_KEY").map(String::as_str),
            Some("xai-secret")
        );
        assert!(missing.is_none());

        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn anthropic_missing_credentials_hint_is_none_when_no_foreign_creds_present() {
        let _lock = env_lock();
        let _openai = EnvVarGuard::set("OPENAI_API_KEY", None);
        let _xai = EnvVarGuard::set("XAI_API_KEY", None);
        let _dashscope = EnvVarGuard::set("DASHSCOPE_API_KEY", None);

        let hint = anthropic_missing_credentials_hint();

        assert!(
            hint.is_none(),
            "no hint should be produced when every foreign provider env var is absent, got {hint:?}"
        );
    }

    #[test]
    fn anthropic_missing_credentials_hint_detects_openai_api_key_and_recommends_openai_prefix() {
        let _lock = env_lock();
        let _openai = EnvVarGuard::set("OPENAI_API_KEY", Some("sk-openrouter-varleg"));
        let _xai = EnvVarGuard::set("XAI_API_KEY", None);
        let _dashscope = EnvVarGuard::set("DASHSCOPE_API_KEY", None);

        let hint = anthropic_missing_credentials_hint()
            .expect("OPENAI_API_KEY presence should produce a hint");

        assert!(hint.contains("OPENAI_API_KEY is set"));
        assert!(hint.contains("OpenAI-compat"));
        assert!(hint.contains("openai/"));
        assert!(hint.contains("OPENAI_BASE_URL"));
    }

    #[test]
    fn anthropic_missing_credentials_hint_detects_xai_api_key() {
        let _lock = env_lock();
        let _openai = EnvVarGuard::set("OPENAI_API_KEY", None);
        let _xai = EnvVarGuard::set("XAI_API_KEY", Some("xai-test-key"));
        let _dashscope = EnvVarGuard::set("DASHSCOPE_API_KEY", None);

        let hint = anthropic_missing_credentials_hint()
            .expect("XAI_API_KEY presence should produce a hint");

        assert!(hint.contains("XAI_API_KEY is set"));
        assert!(hint.contains("xAI"));
        assert!(hint.contains("grok"));
    }

    #[test]
    fn anthropic_missing_credentials_hint_detects_dashscope_api_key() {
        let _lock = env_lock();
        let _openai = EnvVarGuard::set("OPENAI_API_KEY", None);
        let _xai = EnvVarGuard::set("XAI_API_KEY", None);
        let _dashscope = EnvVarGuard::set("DASHSCOPE_API_KEY", Some("sk-dashscope-test"));

        let hint = anthropic_missing_credentials_hint()
            .expect("DASHSCOPE_API_KEY presence should produce a hint");

        assert!(hint.contains("DASHSCOPE_API_KEY is set"));
        assert!(hint.contains("DashScope"));
        assert!(hint.contains("qwen"));
    }

    #[test]
    fn anthropic_missing_credentials_hint_prefers_openai_when_multiple_foreign_creds_set() {
        let _lock = env_lock();
        let _openai = EnvVarGuard::set("OPENAI_API_KEY", Some("sk-openrouter-varleg"));
        let _xai = EnvVarGuard::set("XAI_API_KEY", Some("xai-test-key"));
        let _dashscope = EnvVarGuard::set("DASHSCOPE_API_KEY", Some("sk-dashscope-test"));

        let hint = anthropic_missing_credentials_hint()
            .expect("multiple foreign creds should still produce a hint");

        assert!(hint.contains("OPENAI_API_KEY"));
        assert!(!hint.contains("XAI_API_KEY"));
    }

    #[test]
    fn anthropic_missing_credentials_builds_error_with_canonical_env_vars_and_no_hint_when_clean() {
        let _lock = env_lock();
        let _openai = EnvVarGuard::set("OPENAI_API_KEY", None);
        let _xai = EnvVarGuard::set("XAI_API_KEY", None);
        let _dashscope = EnvVarGuard::set("DASHSCOPE_API_KEY", None);

        let error = anthropic_missing_credentials();

        match &error {
            ApiError::MissingCredentials {
                provider,
                env_vars,
                hint,
            } => {
                assert_eq!(*provider, "Anthropic");
                assert_eq!(
                    *env_vars,
                    &[
                        "ANTHROPIC_API_KEY",
                        "CLAUDE_CODE_OAUTH_TOKEN",
                        "PROXY_AUTH_TOKEN"
                    ]
                );
                assert!(hint.is_none());
            }
            other => panic!("expected MissingCredentials variant, got {other:?}"),
        }
        let rendered = error.to_string();
        assert!(!rendered.contains(" — hint: "));
    }

    #[test]
    fn anthropic_missing_credentials_builds_error_with_hint_when_openai_key_is_set() {
        let _lock = env_lock();
        let _openai = EnvVarGuard::set("OPENAI_API_KEY", Some("sk-openrouter-varleg"));
        let _xai = EnvVarGuard::set("XAI_API_KEY", None);
        let _dashscope = EnvVarGuard::set("DASHSCOPE_API_KEY", None);

        let error = anthropic_missing_credentials();

        match &error {
            ApiError::MissingCredentials {
                provider,
                env_vars,
                hint,
            } => {
                assert_eq!(*provider, "Anthropic");
                assert_eq!(
                    *env_vars,
                    &[
                        "ANTHROPIC_API_KEY",
                        "CLAUDE_CODE_OAUTH_TOKEN",
                        "PROXY_AUTH_TOKEN"
                    ]
                );
                let hint_value = hint.as_deref().expect("hint should be populated");
                assert!(hint_value.contains("OPENAI_API_KEY is set"));
            }
            other => panic!("expected MissingCredentials variant, got {other:?}"),
        }
        let rendered = error.to_string();
        assert!(rendered.starts_with("missing Anthropic credentials;"));
        assert!(rendered.contains(" — hint: I see OPENAI_API_KEY is set"));
    }

    #[test]
    fn anthropic_missing_credentials_hint_ignores_empty_string_values() {
        let _lock = env_lock();
        let _openai = EnvVarGuard::set("OPENAI_API_KEY", Some(""));
        let _xai = EnvVarGuard::set("XAI_API_KEY", None);
        let _dashscope = EnvVarGuard::set("DASHSCOPE_API_KEY", None);

        let hint = anthropic_missing_credentials_hint();

        assert!(hint.is_none());
    }

    #[test]
    fn maps_provider_kind_to_model_family_identity() {
        let anthropic = ProviderKind::Anthropic;
        let openai = ProviderKind::OpenAi;
        let xai = ProviderKind::Xai;
        let codex = ProviderKind::Codex;
        let gemini = ProviderKind::Gemini;

        assert_eq!(
            model_family_identity_for_kind(anthropic),
            runtime::ModelFamilyIdentity::Claude
        );
        assert_eq!(
            model_family_identity_for_kind(openai),
            runtime::ModelFamilyIdentity::Generic
        );
        assert_eq!(
            model_family_identity_for_kind(xai),
            runtime::ModelFamilyIdentity::Generic
        );
        assert_eq!(
            model_family_identity_for_kind(codex),
            runtime::ModelFamilyIdentity::Generic
        );
        assert_eq!(
            model_family_identity_for_kind(gemini),
            runtime::ModelFamilyIdentity::Generic
        );
    }

    #[test]
    fn maps_model_name_to_model_family_identity() {
        assert_eq!(
            model_family_identity_for("claude-opus-4-6"),
            runtime::ModelFamilyIdentity::Claude
        );
        assert_eq!(
            model_family_identity_for("openai/gpt-4.1-mini"),
            runtime::ModelFamilyIdentity::Generic
        );
        assert_eq!(
            model_family_identity_for("grok-3"),
            runtime::ModelFamilyIdentity::Generic
        );
    }

    #[test]
    fn detect_provider_kind_heuristic_covers_prefixes() {
        assert_eq!(
            detect_provider_kind("claude-sonnet-4-6"),
            ProviderKind::Anthropic
        );
        assert_eq!(
            detect_provider_kind("openai/gpt-4.1-mini"),
            ProviderKind::OpenAi
        );
        assert_eq!(detect_provider_kind("grok-3"), ProviderKind::Xai);
        assert_eq!(
            detect_provider_kind("gemini/gemini-pro"),
            ProviderKind::Gemini
        );
        assert_eq!(
            detect_provider_kind("deepseek-v4-pro"),
            ProviderKind::OpenAi
        );
    }
}
