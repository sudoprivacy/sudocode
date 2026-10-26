use plugins::{PluginTool, PluginToolDefinition, PluginToolPermission};
use runtime::ToolDispatchContext;
use serde_json::json;
use tools::GlobalToolRegistry;

#[test]
fn plugin_context_child() {
    if std::env::var("SUDOCODE_PLUGIN_ID").as_deref() == Ok("registry-context@local") {
        println!(
            "HOST_INVOCATION={}",
            std::env::var("SUDOCODE_TOOL_CALL_ID").unwrap_or_else(|_| "absent".to_string())
        );
    }
}

#[test]
fn registry_passes_context_to_the_real_plugin_process() {
    let tool = PluginTool::new(
        "registry-context@local",
        "registry-context",
        PluginToolDefinition {
            name: "native_control_probe".to_string(),
            description: Some("probe host invocation propagation".to_string()),
            input_schema: json!({"type": "object"}),
        },
        std::env::current_exe()
            .expect("test executable")
            .to_string_lossy()
            .to_string(),
        vec![
            "--exact".to_string(),
            "plugin_context_child".to_string(),
            "--nocapture".to_string(),
        ],
        PluginToolPermission::ReadOnly,
        None,
    );
    let registry = GlobalToolRegistry::with_plugin_tools(vec![tool]).expect("registry");
    let context = ToolDispatchContext {
        tool_use_id: Some("actual-runtime-tool-42".to_string()),
        ..ToolDispatchContext::default()
    };
    let output = registry
        .execute_with_abort_and_context("native_control_probe", &json!({}), None, Some(&context))
        .expect("dispatch with host context");
    assert!(output.contains("HOST_INVOCATION=actual-runtime-tool-42"));
    let output = registry
        .execute("native_control_probe", &json!({}))
        .expect("context-free dispatch");
    assert!(output.contains("HOST_INVOCATION=absent"));
    assert!(!output.contains("actual-runtime-tool-42"));
}
