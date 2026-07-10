//! Integration tests for the `Agent(permission_mode = "bubble")`
//! parameter added in Commit 12.
//!
//! ## What this locks in (long-workflow shape)
//!
//! `permission_mode` is currently a documentation/param-only signal:
//! the sub-agent's runtime behavior for permission prompts already
//! bubbles up to the parent process's `PermissionPrompter` — this
//! commit gives the LLM a named way to REQUEST that (matching
//! CC-fork's schema) and validates that the request is accepted.
//!
//! 1. **Schema exposure** — the `Agent` tool's JSON schema advertises
//!    `permission_mode` with an enum limited to `["bubble"]`, so
//!    the model can auto-complete it correctly.
//! 2. **Input parsing** — an Agent-tool JSON payload with
//!    `permission_mode: "bubble"` deserializes without error and
//!    reaches the runtime.
//! 3. **Absent-field parity** — omitting the field parses too
//!    (default = bubble behavior).
//! 4. **Unknown-value tolerance** — passing an unrecognised value
//!    (e.g., `permission_mode: "auto"`) still parses; the runtime
//!    silently ignores unknown modes rather than erroring, so a
//!    forward-compat parent + backward-compat child stay
//!    interoperable.

use tools::mvp_tool_specs;

fn agent_tool_spec_schema() -> serde_json::Value {
    let specs = mvp_tool_specs();
    let agent = specs
        .into_iter()
        .find(|s| s.name == "Agent")
        .expect("Agent tool exists");
    agent.input_schema.clone()
}

#[test]
fn agent_schema_advertises_permission_mode_field_with_bubble_enum() {
    let schema = agent_tool_spec_schema();
    let props = &schema["properties"];
    let pm = &props["permission_mode"];
    assert_eq!(pm["type"].as_str(), Some("string"));
    let variants = pm["enum"].as_array().expect("enum listed");
    let variant_strings: Vec<&str> = variants.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        variant_strings.contains(&"bubble"),
        "permission_mode enum MUST include `bubble` — got: {variant_strings:?}"
    );
    // Description must mention that it maps prompts back to the parent.
    let desc = pm["description"].as_str().unwrap_or_default();
    assert!(
        desc.contains("parent"),
        "permission_mode description must mention `parent` semantics; got: {desc}"
    );
}

#[test]
fn agent_schema_permission_mode_is_optional() {
    let schema = agent_tool_spec_schema();
    let required = schema["required"].as_array().expect("required list");
    let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        !required_names.contains(&"permission_mode"),
        "permission_mode MUST be optional; got required list: {required_names:?}"
    );
}

// The dispatch path (execute_tool "Agent") is exercised end-to-end
// via the PTY live tests (see pty_agent_permission_bubble.rs). Here
// we just prove the input structure round-trips — that's the
// contract this commit is protecting at the LLM boundary.

#[test]
fn agent_tool_accepts_bubble_via_execute_tool_dispatch() {
    // Call the tool with a bubble-mode payload. We expect Ok(_)
    // OR a runtime error unrelated to schema (e.g., "prompt must
    // not be empty"). The forbidden outcome is a JSON parse
    // failure that says the field is unknown or the enum is wrong.
    let input = serde_json::json!({
        "description": "test bubble",
        "prompt": "return the string OK and exit",
        "run_in_background": false,
        "permission_mode": "bubble",
    });
    let outcome = tools::execute_tool("Agent", &input);
    match outcome {
        Ok(_) => {}
        Err(e) => {
            assert!(
                !e.to_lowercase().contains("unknown field") && !e.contains("permission_mode"),
                "unexpected schema-level error for bubble mode: {e}"
            );
        }
    }
}

#[test]
fn agent_tool_accepts_unknown_permission_mode_without_erroring() {
    // Forward compat: a future value like "sandbox" should NOT
    // cause the Agent tool to reject the payload — we silently
    // ignore unknown modes so an older sudocode can still dispatch
    // an Agent call authored against a newer schema.
    let input = serde_json::json!({
        "description": "future value",
        "prompt": "return the string OK and exit",
        "run_in_background": false,
        "permission_mode": "sandbox-future-mode",
    });
    let outcome = tools::execute_tool("Agent", &input);
    if let Err(e) = outcome {
        assert!(
            !e.contains("permission_mode"),
            "unknown permission_mode value should be ignored, not surfaced as error; got: {e}"
        );
    }
}
