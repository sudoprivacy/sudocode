//! `models.<alias>` pass-through fields (`extraBody`, `maxOutputTokens`,
//! `contextWindow`) parsed out of `sudocode.json`.
//!
//! The field is opaque pass-through data for the outbound request body, so
//! the parser has to hand it back verbatim — including float literals, which
//! the hand-rolled `crate::json` parser cannot represent (it is re-read with
//! serde_json for exactly this reason). These cases pin that contract, plus
//! the "absent means empty, and the rest of the entry is unaffected" default
//! every existing config relies on.

use std::fs;

use runtime::ConfigLoader;

fn temp_config_home(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "sudocode-extra-body-{label}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("create temp config home");
    path
}

fn write_config(label: &str, contents: &str) -> std::path::PathBuf {
    let home = temp_config_home(label);
    fs::write(home.join("sudocode.json"), contents).expect("write sudocode.json");
    home
}

const CONFIG_WITH_EXTRA_BODY: &str = r#"{
  "auth_modes": {
    "api-key": {
      "dashscope": {
        "baseUrl": "https://dashscope.aliyuncs.com/compatible-mode/v1",
        "apiKey": "test-key"
      }
    }
  },
  "models": {
    "qwen-fast": {
      "alias": "qwen-fast",
      "name": "qwen3.7-flash",
      "input": ["text"],
      "providers": {
        "api-key": { "provider": "dashscope", "model": "qwen3.7-flash", "api": "openai-completions" }
      },
      "maxOutputTokens": 32768,
      "contextWindow": 1000000,
      "extraBody": {
        "enable_thinking": false,
        "thinking_budget": 0,
        "temperature": 0.6,
        "response_format": { "type": "text" },
        "stop_sequences": ["<eot>"],
        "trace": null
      }
    },
    "qwen-deep": {
      "alias": "qwen-deep",
      "name": "qwen3.8-max",
      "input": ["text"],
      "providers": {
        "api-key": { "provider": "dashscope", "model": "qwen3.8-max", "api": "openai-completions" }
      }
    }
  }
}"#;

#[test]
fn extra_body_is_parsed_verbatim_per_model() {
    let home = write_config("verbatim", CONFIG_WITH_EXTRA_BODY);
    let config = ConfigLoader::new(&home, &home)
        .load_sudocode_config()
        .expect("config with extraBody should load");

    let fast = config.models.get("qwen-fast").expect("qwen-fast entry");
    assert_eq!(
        fast.extra_body.get("enable_thinking"),
        Some(&serde_json::json!(false))
    );
    assert_eq!(
        fast.extra_body.get("thinking_budget"),
        Some(&serde_json::json!(0))
    );
    // Floats survive: the runtime's own JSON value type is integer-only, so
    // this is the case that regresses first if the serde_json pass is lost.
    assert_eq!(
        fast.extra_body.get("temperature"),
        Some(&serde_json::json!(0.6))
    );
    assert_eq!(
        fast.extra_body.get("response_format"),
        Some(&serde_json::json!({"type": "text"}))
    );
    assert_eq!(
        fast.extra_body.get("stop_sequences"),
        Some(&serde_json::json!(["<eot>"]))
    );
    assert_eq!(fast.extra_body.get("trace"), Some(&serde_json::Value::Null));

    // Token-limit overrides ride on the same entry.
    assert_eq!(fast.max_output_tokens, Some(32_768));
    assert_eq!(fast.context_window, Some(1_000_000));

    // The rest of the entry parses exactly as before.
    assert_eq!(fast.name, "qwen3.7-flash");
    assert_eq!(
        fast.providers
            .get("api-key")
            .map(|mapping| mapping.model.as_str()),
        Some("qwen3.7-flash")
    );

    // Sibling model without the field is untouched.
    let deep = config.models.get("qwen-deep").expect("qwen-deep entry");
    assert!(
        deep.extra_body.is_empty(),
        "no extraBody means an empty map, not an inherited one: {:?}",
        deep.extra_body
    );
    assert_eq!(deep.max_output_tokens, None);
    assert_eq!(deep.context_window, None);
}

#[test]
fn config_without_extra_body_parses_unchanged() {
    let home = write_config("absent", runtime::config::SAMPLE_SUDOCODE_JSON);
    let config = ConfigLoader::new(&home, &home)
        .load_sudocode_config()
        .expect("sample config should load");

    assert!(!config.models.is_empty(), "sample config declares models");
    for (alias, entry) in &config.models {
        assert!(
            entry.extra_body.is_empty(),
            "model {alias} should have no extraBody"
        );
        assert_eq!(entry.max_output_tokens, None, "model {alias}");
        assert_eq!(entry.context_window, None, "model {alias}");
    }
}

#[test]
fn non_object_extra_body_is_a_config_error() {
    let home = write_config(
        "not-an-object",
        r#"{
  "models": {
    "qwen-fast": {
      "name": "qwen3.7-flash",
      "providers": {},
      "extraBody": "enable_thinking=false"
    }
  }
}"#,
    );
    let error = ConfigLoader::new(&home, &home)
        .load_sudocode_config()
        .expect_err("a string extraBody should be rejected");
    let message = error.to_string();
    assert!(
        message.contains("extraBody"),
        "error should name the offending field: {message}"
    );
}
