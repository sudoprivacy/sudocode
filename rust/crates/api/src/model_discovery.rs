//! Discovery uses exactly the provider selected for inference.

use crate::{ApiFormat, Credential, ResolvedProvider};
use runtime::model_discovery::{DiscoverySource, ModelCatalog};
use std::collections::BTreeMap;

#[must_use]
pub fn model_catalog_for_resolved(resolved: &ResolvedProvider) -> Option<ModelCatalog> {
    Some(ModelCatalog::open(
        &runtime::default_config_home(),
        discovery_source(resolved)?,
    ))
}

fn discovery_source(resolved: &ResolvedProvider) -> Option<DiscoverySource> {
    let mut headers = BTreeMap::new();
    match &resolved.credential {
        Credential::ApiKey(key) => {
            if resolved.api_format == ApiFormat::AnthropicMessages {
                headers.insert("x-api-key".into(), key.clone());
            } else {
                headers.insert("authorization".into(), format!("Bearer {key}"));
            }
        }
        Credential::Token(token) => {
            headers.insert("authorization".into(), format!("Bearer {token}"));
            if resolved.api_format == ApiFormat::AnthropicMessages {
                headers.insert("anthropic-beta".into(), "oauth-2025-04-20".into());
            }
        }
        // Provider-owned credential-file refresh remains on its native path.
        Credential::AuthFile(_) => return None,
        Credential::None => {}
    }
    if resolved.api_format == ApiFormat::GeminiGenerateContent {
        return None;
    }
    if resolved.api_format == ApiFormat::AnthropicMessages {
        headers.insert("anthropic-version".into(), "2023-06-01".into());
    }
    let base = resolved.base_url.trim_end_matches('/');
    let models_url =
        if resolved.api_format == ApiFormat::AnthropicMessages && !base.ends_with("/v1") {
            format!("{base}/v1/models")
        } else {
            format!("{base}/models")
        };
    Some(DiscoverySource {
        models_url,
        headers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AuthMode, ProviderConnectionConfig, SudoCodeConfig};

    #[test]
    fn model_discovery_follows_the_account_selected_for_inference() {
        let mut config = SudoCodeConfig::default();
        let connections = ["fujitoken", "another-account"]
            .into_iter()
            .map(|name| {
                (
                    name.into(),
                    ProviderConnectionConfig {
                        base_url: format!("https://{name}.example.test/v1"),
                        api_key: Some(format!("{name}-key")),
                        api_key_env: None,
                        token: None,
                        token_env: None,
                        auth_file: None,
                    },
                )
            })
            .collect();
        config.auth_modes.insert("proxy".into(), connections);
        for account in ["fujitoken", "another-account"] {
            config.selected_account = Some(account.into());
            for model in ["claude-unseen-2040", "gpt-unseen-2040"] {
                let resolved =
                    crate::resolve_provider_from_config(model, Some(AuthMode::Proxy), &config)
                        .unwrap();
                let source = discovery_source(&resolved).unwrap();
                assert_eq!(
                    source.models_url,
                    format!("https://{account}.example.test/v1/models")
                );
                if resolved.api_format == ApiFormat::AnthropicMessages {
                    assert_eq!(source.headers["x-api-key"], format!("{account}-key"));
                    assert_eq!(source.headers["anthropic-version"], "2023-06-01");
                } else {
                    assert_eq!(
                        source.headers["authorization"],
                        format!("Bearer {account}-key")
                    );
                }
            }
        }
    }
}
