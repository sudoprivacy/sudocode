use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use plugins::PluginLoadOutcome;
use runtime::{McpServerManager, McpTool, PermissionMode, ToolError};
use serde_json::json;
use tools::RuntimeToolDefinition;

pub(crate) type RuntimePluginStateBuildOutput = (
    Option<Arc<Mutex<RuntimeMcpState>>>,
    Vec<RuntimeToolDefinition>,
);

pub(crate) struct RuntimeMcpState {
    pub(crate) runtime: tokio::runtime::Runtime,
    pub(crate) manager: McpServerManager,
    pub(crate) pending_servers: Vec<String>,
    pub(crate) degraded_report: Option<runtime::McpDegradedReport>,
}

impl RuntimeMcpState {
    pub(crate) fn new(
        runtime_config: &runtime::RuntimeConfig,
        plugin_load_outcome: &PluginLoadOutcome,
    ) -> Result<Option<(Self, runtime::McpToolDiscoveryReport)>, Box<dyn std::error::Error>> {
        let servers = merged_mcp_servers(runtime_config, plugin_load_outcome)?;
        let mut manager = McpServerManager::from_servers(&servers);
        if manager.server_names().is_empty() && manager.unsupported_servers().is_empty() {
            return Ok(None);
        }

        let runtime = tokio::runtime::Runtime::new()?;
        let discovery = runtime.block_on(manager.discover_tools_best_effort());
        let pending_servers = discovery
            .failed_servers
            .iter()
            .map(|failure| failure.server_name.clone())
            .chain(
                discovery
                    .unsupported_servers
                    .iter()
                    .map(|server| server.server_name.clone()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let available_tools = discovery
            .tools
            .iter()
            .map(|tool| tool.qualified_name.clone())
            .collect::<Vec<_>>();
        let failed_server_names = pending_servers.iter().cloned().collect::<BTreeSet<_>>();
        let working_servers = manager
            .server_names()
            .into_iter()
            .filter(|server_name| !failed_server_names.contains(server_name))
            .collect::<Vec<_>>();
        let failed_servers =
            discovery
                .failed_servers
                .iter()
                .map(|failure| runtime::McpFailedServer {
                    server_name: failure.server_name.clone(),
                    phase: runtime::McpLifecyclePhase::ToolDiscovery,
                    error: runtime::McpErrorSurface::new(
                        runtime::McpLifecyclePhase::ToolDiscovery,
                        Some(failure.server_name.clone()),
                        failure.error.clone(),
                        std::collections::BTreeMap::new(),
                        true,
                    ),
                })
                .chain(discovery.unsupported_servers.iter().map(|server| {
                    runtime::McpFailedServer {
                        server_name: server.server_name.clone(),
                        phase: runtime::McpLifecyclePhase::ServerRegistration,
                        error: runtime::McpErrorSurface::new(
                            runtime::McpLifecyclePhase::ServerRegistration,
                            Some(server.server_name.clone()),
                            server.reason.clone(),
                            std::collections::BTreeMap::from([(
                                "transport".to_string(),
                                format!("{:?}", server.transport).to_ascii_lowercase(),
                            )]),
                            false,
                        ),
                    }
                }))
                .collect::<Vec<_>>();
        let degraded_report = (!failed_servers.is_empty()).then(|| {
            runtime::McpDegradedReport::new(
                working_servers,
                failed_servers,
                available_tools.clone(),
                available_tools,
            )
        });

        Ok(Some((
            Self {
                runtime,
                manager,
                pending_servers,
                degraded_report,
            },
            discovery,
        )))
    }

    pub(crate) fn shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.block_on(self.manager.shutdown())?;
        Ok(())
    }

    pub(crate) fn pending_servers(&self) -> Option<Vec<String>> {
        (!self.pending_servers.is_empty()).then(|| self.pending_servers.clone())
    }

    pub(crate) fn degraded_report(&self) -> Option<runtime::McpDegradedReport> {
        self.degraded_report.clone()
    }

    pub(crate) fn server_names(&self) -> Vec<String> {
        self.manager.server_names()
    }

    /// Drives `future` to completion on a dedicated OS thread instead of
    /// calling `self.runtime.block_on` on the caller's thread.
    ///
    /// The interactive and ACP tool loops run synchronously inside an outer
    /// `tokio_runtime.block_on(run_turn)` (multi-thread runtime), so the MCP
    /// tool methods below are reached from a tokio worker thread that has
    /// already entered a runtime. Calling `self.runtime.block_on` there panics
    /// with "Cannot start a runtime from within a runtime". A scoped thread has
    /// no entered runtime, so the nested `block_on` is safe. This mirrors the
    /// thread isolation already used by `runtime::mcp_tool_bridge`'s
    /// `spawn_tool_call`. `scope` joins the thread before returning, preserving
    /// the original blocking semantics.
    fn block_on_isolated<F>(runtime: &tokio::runtime::Runtime, future: F) -> F::Output
    where
        F: std::future::Future + Send,
        F::Output: Send,
    {
        std::thread::scope(|scope| {
            scope
                .spawn(|| runtime.block_on(future))
                .join()
                .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
        })
    }

    pub(crate) fn call_tool(
        &mut self,
        qualified_tool_name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<String, ToolError> {
        let response = Self::block_on_isolated(
            &self.runtime,
            self.manager.call_tool(qualified_tool_name, arguments),
        )
        .map_err(|error| ToolError::new(error.to_string()))?;
        if let Some(error) = response.error {
            return Err(ToolError::new(format!(
                "MCP tool `{qualified_tool_name}` returned JSON-RPC error: {} ({})",
                error.message, error.code
            )));
        }

        let result = response.result.ok_or_else(|| {
            ToolError::new(format!(
                "MCP tool `{qualified_tool_name}` returned no result payload"
            ))
        })?;
        serde_json::to_string_pretty(&result).map_err(|error| ToolError::new(error.to_string()))
    }

    pub(crate) fn list_resources_for_server(
        &mut self,
        server_name: &str,
    ) -> Result<String, ToolError> {
        let result =
            Self::block_on_isolated(&self.runtime, self.manager.list_resources(server_name))
                .map_err(|error| ToolError::new(error.to_string()))?;
        serde_json::to_string_pretty(&json!({
            "server": server_name,
            "resources": result.resources,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    pub(crate) fn list_resources_for_all_servers(&mut self) -> Result<String, ToolError> {
        let mut resources = Vec::new();
        let mut failures = Vec::new();

        for server_name in self.server_names() {
            match Self::block_on_isolated(&self.runtime, self.manager.list_resources(&server_name))
            {
                Ok(result) => resources.push(json!({
                    "server": server_name,
                    "resources": result.resources,
                })),
                Err(error) => failures.push(json!({
                    "server": server_name,
                    "error": error.to_string(),
                })),
            }
        }

        if resources.is_empty() && !failures.is_empty() {
            let message = failures
                .iter()
                .filter_map(|failure| failure.get("error").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ToolError::new(message));
        }

        serde_json::to_string_pretty(&json!({
            "resources": resources,
            "failures": failures,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    pub(crate) fn read_resource(
        &mut self,
        server_name: &str,
        uri: &str,
    ) -> Result<String, ToolError> {
        let result =
            Self::block_on_isolated(&self.runtime, self.manager.read_resource(server_name, uri))
                .map_err(|error| ToolError::new(error.to_string()))?;
        serde_json::to_string_pretty(&json!({
            "server": server_name,
            "contents": result.contents,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }
}

pub(crate) fn merged_mcp_servers(
    runtime_config: &runtime::RuntimeConfig,
    plugin_load_outcome: &PluginLoadOutcome,
) -> Result<BTreeMap<String, runtime::ScopedMcpServerConfig>, Box<dyn std::error::Error>> {
    let mut servers = runtime_config.mcp().servers().clone();
    for plugin in &plugin_load_outcome.loaded_plugins {
        if !plugin.summary.enabled {
            continue;
        }
        for path in &plugin.mcp_config_paths {
            let plugin_servers = runtime::load_plugin_mcp_servers(path)?;
            for (server_name, server_config) in plugin_servers {
                servers.entry(server_name).or_insert(server_config);
            }
        }
    }
    Ok(servers)
}

pub(crate) fn build_runtime_mcp_state(
    runtime_config: &runtime::RuntimeConfig,
    plugin_load_outcome: &PluginLoadOutcome,
) -> Result<RuntimePluginStateBuildOutput, Box<dyn std::error::Error>> {
    let Some((mcp_state, discovery)) = RuntimeMcpState::new(runtime_config, plugin_load_outcome)?
    else {
        return Ok((None, Vec::new()));
    };

    let mut runtime_tools = discovery
        .tools
        .iter()
        .map(mcp_runtime_tool_definition)
        .collect::<Vec<_>>();
    if !mcp_state.server_names().is_empty() {
        runtime_tools.extend(mcp_wrapper_tool_definitions());
    }

    Ok((Some(Arc::new(Mutex::new(mcp_state))), runtime_tools))
}

pub(crate) fn mcp_runtime_tool_definition(tool: &runtime::ManagedMcpTool) -> RuntimeToolDefinition {
    RuntimeToolDefinition {
        name: tool.qualified_name.clone(),
        description: Some(
            tool.tool
                .description
                .clone()
                .unwrap_or_else(|| format!("Invoke MCP tool `{}`.", tool.qualified_name)),
        ),
        input_schema: tool
            .tool
            .input_schema
            .clone()
            .unwrap_or_else(|| json!({ "type": "object", "additionalProperties": true })),
        required_permission: permission_mode_for_mcp_tool(&tool.tool),
    }
}

pub(crate) fn mcp_wrapper_tool_definitions() -> Vec<RuntimeToolDefinition> {
    vec![
        RuntimeToolDefinition {
            name: "MCPTool".to_string(),
            description: Some(
                "Call a configured MCP tool by its qualified name and JSON arguments.".to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "qualifiedName": { "type": "string" },
                    "arguments": {}
                },
                "required": ["qualifiedName"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        RuntimeToolDefinition {
            name: "ListMcpResourcesTool".to_string(),
            description: Some(
                "List MCP resources from one configured server or from every connected server."
                    .to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        RuntimeToolDefinition {
            name: "ReadMcpResourceTool".to_string(),
            description: Some("Read a specific MCP resource from a configured server.".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" }
                },
                "required": ["server", "uri"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
    ]
}

pub(crate) fn permission_mode_for_mcp_tool(tool: &McpTool) -> PermissionMode {
    let read_only = mcp_annotation_flag(tool, "readOnlyHint");
    let destructive = mcp_annotation_flag(tool, "destructiveHint");
    let open_world = mcp_annotation_flag(tool, "openWorldHint");

    if read_only && !destructive && !open_world {
        PermissionMode::ReadOnly
    } else if destructive || open_world {
        PermissionMode::DangerFullAccess
    } else {
        PermissionMode::WorkspaceWrite
    }
}

pub(crate) fn mcp_annotation_flag(tool: &McpTool, key: &str) -> bool {
    tool.annotations
        .as_ref()
        .and_then(|annotations| annotations.get(key))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use plugins::{
        LoadedPlugin, PluginCapabilityMetadata, PluginCapabilitySummary, PluginKind,
        PluginLoadOutcome, PluginMetadata, PluginSummary,
    };

    use super::merged_mcp_servers;

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sudocode-cli-mcp-{label}-{nanos}"))
    }

    fn loaded_plugin_with_mcp_path(path: PathBuf) -> LoadedPlugin {
        loaded_plugin_with_mcp_path_enabled(path, true)
    }

    fn loaded_plugin_with_mcp_path_enabled(path: PathBuf, enabled: bool) -> LoadedPlugin {
        let metadata = PluginMetadata {
            id: "plugin-demo@external".to_string(),
            name: "plugin-demo".to_string(),
            version: "1.0.0".to_string(),
            description: "Plugin demo".to_string(),
            kind: PluginKind::External,
            source: "external".to_string(),
            default_enabled: true,
            root: path.parent().map(PathBuf::from),
            display_name: None,
        };
        LoadedPlugin {
            summary: PluginSummary {
                metadata: metadata.clone(),
                enabled,
            },
            root: metadata.root.clone(),
            kind: metadata.kind,
            source: metadata.source.clone(),
            capabilities: PluginCapabilityMetadata::default(),
            skill_roots: Vec::new(),
            mcp_config_paths: vec![path],
            app_config_paths: Vec::new(),
            capability_summary: PluginCapabilitySummary {
                plugin_id: metadata.id,
                display_name: metadata.name,
                description: metadata.description,
                tool_count: 0,
                pre_tool_hook_count: 0,
                post_tool_hook_count: 0,
                post_tool_use_failure_hook_count: 0,
                has_skills: false,
                has_mcp_servers: true,
                has_apps: false,
            },
        }
    }

    #[test]
    fn merged_mcp_servers_keeps_runtime_config_on_name_collision() {
        let root = temp_dir("merge");
        let cwd = root.join("project");
        let home = root.join("home");
        let plugin_root = root.join("plugin");
        fs::create_dir_all(&cwd).expect("cwd");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&plugin_root).expect("plugin");
        fs::write(
            home.join("settings.json"),
            r#"{
              "mcpServers": {
                "shared": {"command": "uvx", "args": ["runtime-server"]}
              }
            }"#,
        )
        .expect("runtime config");
        let plugin_mcp = plugin_root.join(".mcp.json");
        fs::write(
            &plugin_mcp,
            r#"{
              "mcpServers": {
                "shared": {"command": "./plugin-server"},
                "plugin-only": {"command": "./plugin-only"}
              }
            }"#,
        )
        .expect("plugin mcp config");

        let runtime_config = runtime::ConfigLoader::new(&cwd, &home)
            .load()
            .expect("runtime config should load");
        let outcome = PluginLoadOutcome {
            loaded_plugins: vec![loaded_plugin_with_mcp_path(plugin_mcp)],
            failures: Vec::new(),
        };

        let merged = merged_mcp_servers(&runtime_config, &outcome).expect("merge should work");
        match &merged.get("shared").expect("shared server").config {
            runtime::McpServerConfig::Stdio(stdio) => {
                assert_eq!(stdio.command, "uvx");
                assert_eq!(stdio.args, vec!["runtime-server"]);
            }
            other => panic!("expected stdio config, got {other:?}"),
        }
        match &merged
            .get("plugin-only")
            .expect("plugin-only server")
            .config
        {
            runtime::McpServerConfig::Stdio(stdio) => {
                assert_eq!(
                    stdio.command,
                    plugin_root.join("plugin-only").display().to_string()
                );
            }
            other => panic!("expected stdio config, got {other:?}"),
        }

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn merged_mcp_servers_ignores_disabled_plugins() {
        let root = temp_dir("disabled");
        let cwd = root.join("project");
        let home = root.join("home");
        let plugin_root = root.join("plugin");
        fs::create_dir_all(&cwd).expect("cwd");
        fs::create_dir_all(&home).expect("home");
        fs::create_dir_all(&plugin_root).expect("plugin");
        let plugin_mcp = plugin_root.join(".mcp.json");
        fs::write(
            &plugin_mcp,
            r#"{
              "mcpServers": {
                "disabled-only": {"command": "./disabled-server"}
              }
            }"#,
        )
        .expect("plugin mcp config");

        let runtime_config = runtime::ConfigLoader::new(&cwd, &home)
            .load()
            .expect("runtime config should load");
        let outcome = PluginLoadOutcome {
            loaded_plugins: vec![loaded_plugin_with_mcp_path_enabled(plugin_mcp, false)],
            failures: Vec::new(),
        };

        let merged = merged_mcp_servers(&runtime_config, &outcome).expect("merge should work");
        assert!(
            !merged.contains_key("disabled-only"),
            "disabled plugin MCP servers should not be projected"
        );

        fs::remove_dir_all(root).expect("cleanup");
    }
}
