//! `models.<alias>.maxOutputTokens` / `contextWindow` overrides.
//!
//! The model-capabilities table is compiled into the binary, so any model it
//! has never heard of silently inherits the table's `default` entry — and a
//! provider whose real ceiling is lower rejects every request
//! (`qwen-flash` on DashScope: `Range of max_tokens should be [1, 32768]`
//! against sudocode's default 64000). These cases pin the config-side patch.
//!
//! The overrides are process-global (they seed the capabilities SSOT), which
//! is why this file is its own test binary.

use std::collections::BTreeMap;

use api::{
    max_tokens_for_model, model_token_limit, ModelConfigEntry, ModelProviderMapping, SudoCodeConfig,
};

fn config_with(
    alias: &str,
    wire: &str,
    max_output: Option<u32>,
    window: Option<u32>,
) -> SudoCodeConfig {
    let mut providers = BTreeMap::new();
    providers.insert(
        "api-key".to_string(),
        ModelProviderMapping {
            provider: "bailian".to_string(),
            model: wire.to_string(),
            api: Some("openai-completions".to_string()),
        },
    );
    let mut models = BTreeMap::new();
    models.insert(
        alias.to_string(),
        ModelConfigEntry {
            alias: alias.to_string(),
            name: alias.to_string(),
            input: vec!["text".to_string()],
            providers,
            max_output_tokens: max_output,
            context_window: window,
            ..Default::default()
        },
    );
    SudoCodeConfig {
        models,
        ..Default::default()
    }
}

#[test]
fn configured_limits_override_the_compiled_table() {
    // A model the bundled table does not know: without an override it takes
    // the 64k heuristic, which DashScope rejects.
    assert_eq!(max_tokens_for_model("qwen-flash-test-only"), 64_000);

    runtime::model_capabilities::apply_config_limits(&config_with(
        "qwen-flash",
        "qwen-flash-test-only",
        Some(32_768),
        Some(1_000_000),
    ));

    assert_eq!(max_tokens_for_model("qwen-flash-test-only"), 32_768);
    let limit = model_token_limit("qwen-flash-test-only").expect("configured model is known");
    assert_eq!(limit.max_output_tokens, 32_768);
    assert_eq!(limit.context_window_tokens, 1_000_000);
    assert_eq!(
        runtime::model_capabilities::context_window_or_default("qwen-flash-test-only"),
        1_000_000
    );

    // A provider-prefixed wire ID resolves to the same override.
    assert_eq!(
        max_tokens_for_model("dashscope/qwen-flash-test-only"),
        32_768
    );

    // Models the config says nothing about are untouched.
    assert_eq!(max_tokens_for_model("some-other-model"), 64_000);

    // Re-seeding with a config that declares no limits clears the override.
    runtime::model_capabilities::apply_config_limits(&config_with(
        "qwen-flash",
        "qwen-flash-test-only",
        None,
        None,
    ));
    assert_eq!(max_tokens_for_model("qwen-flash-test-only"), 64_000);
}
