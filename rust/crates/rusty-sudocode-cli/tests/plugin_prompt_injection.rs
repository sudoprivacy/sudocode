use plugins::{
    render_plugin_capabilities_section, LoadedPlugin, PluginCapabilityMetadata,
    PluginCapabilitySummary, PluginKind, PluginMetadata, PluginSummary,
};
use runtime::SystemPrompt;

fn make_loaded_plugin(id: &str, name: &str, desc: &str, enabled: bool) -> LoadedPlugin {
    let (pname, source) = id
        .split_once('@')
        .map(|(n, s)| (n.to_string(), s.to_string()))
        .unwrap_or_else(|| (id.to_string(), "test".to_string()));
    LoadedPlugin {
        summary: PluginSummary {
            metadata: PluginMetadata {
                id: id.to_string(),
                name: pname,
                version: "0.1.0".to_string(),
                description: desc.to_string(),
                kind: PluginKind::External,
                source: source.clone(),
                default_enabled: true,
                root: None,
                display_name: None,
            },
            enabled,
        },
        root: None,
        kind: PluginKind::External,
        source,
        capabilities: PluginCapabilityMetadata::default(),
        skill_roots: vec![],
        mcp_config_paths: vec![],
        app_config_paths: vec![],
        capability_summary: PluginCapabilitySummary {
            plugin_id: id.to_string(),
            display_name: name.to_string(),
            description: desc.to_string(),
            tool_count: 1,
            pre_tool_hook_count: 0,
            post_tool_hook_count: 0,
            post_tool_use_failure_hook_count: 0,
            has_skills: false,
            has_mcp_servers: false,
            has_apps: false,
        },
    }
}

#[test]
fn plugin_section_appended_to_dynamic_sections() {
    let plugin = make_loaded_plugin("test-plugin@external", "Test Plugin", "Does testing", true);
    let section = render_plugin_capabilities_section(&[plugin]).unwrap();

    let mut prompt = SystemPrompt::default();
    prompt.dynamic_sections.push(section);

    let dynamic = prompt.dynamic_text();
    assert!(dynamic.contains("# Available SudoCode plugins"));
    assert!(dynamic.contains("Plugin 1"));
    assert!(!dynamic.contains("test-plugin@external"));
    assert!(!dynamic.contains("Test Plugin"));
}

#[test]
fn no_section_injected_when_no_active_plugins() {
    let mut prompt = SystemPrompt::default();
    if let Some(section) = render_plugin_capabilities_section(&[]) {
        prompt.dynamic_sections.push(section);
    }
    assert!(
        !prompt
            .dynamic_text()
            .contains("# Available SudoCode plugins"),
        "no section should appear when there are no active plugins"
    );
}

#[test]
fn disabled_plugin_does_not_land_in_dynamic_sections() {
    let plugin = make_loaded_plugin("off@bundled", "Off Plugin", "Should be absent", false);

    let mut prompt = SystemPrompt::default();
    if let Some(section) = render_plugin_capabilities_section(&[plugin]) {
        prompt.dynamic_sections.push(section);
    }
    assert!(
        !prompt.dynamic_text().contains("off@bundled"),
        "disabled plugin must not appear in dynamic sections"
    );
}
