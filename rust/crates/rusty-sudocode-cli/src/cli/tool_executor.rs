use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use runtime::{
    PermissionMode, PermissionPolicy, QuestionField, QuestionKind, QuestionOption,
    QuestionPromptAnswer, QuestionPromptRequest, QuestionPrompter, ToolError, ToolExecutor,
};
use serde::Deserialize;
use tools::GlobalToolRegistry;

use super::format::format_tool_result;
use crate::render::TerminalRenderer;
use crate::{AllowedToolSet, RuntimeMcpState};

#[derive(Debug, Deserialize)]
pub(crate) struct ToolSearchRequest {
    pub(crate) query: String,
    pub(crate) max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct McpToolRequest {
    #[serde(rename = "qualifiedName")]
    pub(crate) qualified_name: Option<String>,
    pub(crate) tool: Option<String>,
    pub(crate) arguments: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListMcpResourcesRequest {
    pub(crate) server: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReadMcpResourceRequest {
    pub(crate) server: String,
    pub(crate) uri: String,
}

pub(crate) struct CliToolExecutor {
    renderer: TerminalRenderer,
    emit_output: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    spinner_pause: Option<Arc<AtomicBool>>,
    question_prompter: Option<Box<dyn QuestionPrompter>>,
    abort_signal: Option<runtime::HookAbortSignal>,
}

impl CliToolExecutor {
    pub(crate) fn new(
        allowed_tools: Option<AllowedToolSet>,
        emit_output: bool,
        tool_registry: GlobalToolRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            renderer: TerminalRenderer::new(),
            emit_output,
            allowed_tools,
            tool_registry,
            mcp_state,
            spinner_pause: None,
            question_prompter: None,
            abort_signal: None,
        }
    }

    pub(crate) fn set_spinner_pause(&mut self, flag: Arc<AtomicBool>) {
        self.spinner_pause = Some(flag);
    }

    pub(crate) fn set_question_prompter(&mut self, prompter: Box<dyn QuestionPrompter>) {
        self.question_prompter = Some(prompter);
    }

    /// Pause the spinner and clear its line before writing content.
    fn pause_spinner(&self) {
        if let Some(flag) = &self.spinner_pause {
            flag.store(true, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(10));
            let _ = write!(io::stdout(), "\r\x1b[2K");
            let _ = io::stdout().flush();
        }
    }

    /// Resume the spinner after content has been written.
    fn resume_spinner(&self) {
        if let Some(flag) = &self.spinner_pause {
            flag.store(false, Ordering::SeqCst);
        }
    }

    fn execute_search_tool(&self, value: serde_json::Value) -> Result<String, ToolError> {
        let input: ToolSearchRequest = serde_json::from_value(value)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let (pending_mcp_servers, mcp_degraded) =
            self.mcp_state.as_ref().map_or((None, None), |state| {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (state.pending_servers(), state.degraded_report())
            });
        serde_json::to_string_pretty(&self.tool_registry.search(
            &input.query,
            input.max_results.unwrap_or(5),
            pending_mcp_servers,
            mcp_degraded,
        ))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    fn execute_runtime_tool(
        &self,
        tool_name: &str,
        value: serde_json::Value,
    ) -> Result<String, ToolError> {
        let Some(mcp_state) = &self.mcp_state else {
            return Err(ToolError::new(format!(
                "runtime tool `{tool_name}` is unavailable without configured MCP servers"
            )));
        };
        let mut mcp_state = mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match tool_name {
            "MCPTool" => {
                let input: McpToolRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                let qualified_name = input
                    .qualified_name
                    .or(input.tool)
                    .ok_or_else(|| ToolError::new("missing required field `qualifiedName`"))?;
                mcp_state.call_tool(&qualified_name, input.arguments)
            }
            "ListMcpResourcesTool" => {
                let input: ListMcpResourcesRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                match input.server {
                    Some(server_name) => mcp_state.list_resources_for_server(&server_name),
                    None => mcp_state.list_resources_for_all_servers(),
                }
            }
            "ReadMcpResourceTool" => {
                let input: ReadMcpResourceRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                mcp_state.read_resource(&input.server, &input.uri)
            }
            _ => mcp_state.call_tool(tool_name, Some(value)),
        }
    }
}

impl ToolExecutor for CliToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/scode-acp-diag.log")
            {
                let _ = writeln!(
                    f,
                    "[ACP-DIAG] execute tool_name={} prompter_set={}",
                    tool_name,
                    self.question_prompter.is_some()
                );
            }
        }
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(tool_name))
        {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled by the current --allowedTools setting"
            )));
        }
        let value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        if tool_name == "AskUserQuestion" && self.question_prompter.is_some() {
            return self.execute_ask_user_question(value);
        }
        let result = if tool_name == "ToolSearch" {
            self.execute_search_tool(value)
        } else if self.tool_registry.has_runtime_tool(tool_name) {
            self.execute_runtime_tool(tool_name, value)
        } else {
            self.tool_registry
                .execute_with_abort(tool_name, &value, self.abort_signal.as_ref())
                .map_err(ToolError::new)
        };
        match result {
            Ok(output) => {
                if self.emit_output {
                    self.pause_spinner();
                    let interrupted = self
                        .abort_signal
                        .as_ref()
                        .is_some_and(runtime::HookAbortSignal::is_aborted);
                    let formatted = format_tool_result(tool_name, &output, interrupted);
                    writeln!(io::stdout(), "{formatted}")
                        .and_then(|()| io::stdout().flush())
                        .map_err(|error| ToolError::new(error.to_string()))?;
                    self.resume_spinner();
                }
                Ok(output)
            }
            Err(error) => {
                if self.emit_output {
                    self.pause_spinner();
                    let formatted = format_tool_result(tool_name, &error.to_string(), true);
                    writeln!(io::stdout(), "{formatted}")
                        .and_then(|()| io::stdout().flush())
                        .map_err(|error| ToolError::new(error.to_string()))?;
                    self.resume_spinner();
                }
                Err(error)
            }
        }
    }

    fn set_abort_signal(&mut self, abort_signal: runtime::HookAbortSignal) {
        self.abort_signal = Some(abort_signal);
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskUserQuestionCliInput {
    question: Option<String>,
    options: Option<Vec<String>>,
    title: Option<String>,
    description: Option<String>,
    #[serde(default)]
    questions: Vec<AskUserQuestionCliField>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskUserQuestionCliField {
    id: String,
    prompt: String,
    kind: Option<String>,
    required: Option<bool>,
    allow_custom_input: Option<bool>,
    custom_input_hint: Option<String>,
    #[serde(default)]
    options: Vec<AskUserQuestionCliOption>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskUserQuestionCliOption {
    label: String,
    value: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    recommended: Option<bool>,
}

impl CliToolExecutor {
    fn execute_ask_user_question(&mut self, value: serde_json::Value) -> Result<String, ToolError> {
        {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("/tmp/scode-acp-diag.log")
            {
                let _ = writeln!(f, "[ACP-DIAG] execute_ask_user_question invoked");
            }
        }
        let input: AskUserQuestionCliInput = serde_json::from_value(value)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;

        let Some(prompter) = self.question_prompter.as_mut() else {
            return Err(ToolError::new(
                "AskUserQuestion requires an interactive question prompter",
            ));
        };

        let fields = if !input.questions.is_empty() {
            input
                .questions
                .into_iter()
                .map(|field| {
                    let options = field
                        .options
                        .into_iter()
                        .map(|option| QuestionOption {
                            label: option.label,
                            value: option.value,
                            description: option.description,
                            recommended: option.recommended.unwrap_or(false),
                        })
                        .collect::<Vec<_>>();
                    let kind = field
                        .kind
                        .as_deref()
                        .and_then(QuestionKind::from_str)
                        .unwrap_or_else(|| {
                            if options.is_empty() {
                                QuestionKind::Text
                            } else {
                                QuestionKind::SingleSelect
                            }
                        });
                    QuestionField {
                        id: field.id,
                        prompt: field.prompt,
                        kind,
                        required: field.required.unwrap_or(true),
                        allow_custom_input: field.allow_custom_input.unwrap_or(false),
                        custom_input_hint: field.custom_input_hint,
                        options,
                    }
                })
                .collect::<Vec<_>>()
        } else {
            let prompt = input
                .question
                .map(|question| question.trim().to_string())
                .filter(|question| !question.is_empty())
                .ok_or_else(|| ToolError::new("question or questions is required"))?;
            let options = input
                .options
                .unwrap_or_default()
                .into_iter()
                .filter(|option| !option.trim().is_empty())
                .map(|option| QuestionOption {
                    label: option.clone(),
                    value: option,
                    description: None,
                    recommended: false,
                })
                .collect::<Vec<_>>();
            let kind = if options.is_empty() {
                QuestionKind::Text
            } else {
                QuestionKind::SingleSelect
            };
            vec![QuestionField {
                id: "q1".to_string(),
                prompt,
                kind,
                required: true,
                allow_custom_input: options.is_empty(),
                custom_input_hint: None,
                options,
            }]
        };

        let request = QuestionPromptRequest {
            title: input.title,
            description: input.description,
            fields,
        };

        let answers = prompter.ask(&request).map_err(ToolError::new)?;
        serde_json::to_string_pretty(&serde_json::json!({
            "status": "answered",
            "title": request.title,
            "description": request.description,
            "questions": request.fields.iter().map(|field| serde_json::json!({
              "id": field.id,
              "prompt": field.prompt,
              "kind": field.kind.as_str(),
              "required": field.required,
              "allowCustomInput": field.allow_custom_input,
              "customInputHint": field.custom_input_hint,
              "options": field.options.iter().map(|option| serde_json::json!({
                "label": option.label,
                "value": option.value,
                "description": option.description,
                "recommended": option.recommended,
              })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "answers": answers.iter().map(|answer| serde_json::json!({
              "id": answer.id,
              "value": answer.value,
              "label": answer.label,
            })).collect::<Vec<_>>(),
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }
}

pub(crate) fn permission_policy(
    mode: PermissionMode,
    feature_config: &runtime::RuntimeFeatureConfig,
    tool_registry: &GlobalToolRegistry,
    cwd: &std::path::Path,
) -> Result<PermissionPolicy, String> {
    let memory_dir = runtime::memory::default_memory_dir_for(cwd);
    Ok(tool_registry.permission_specs(None)?.into_iter().fold(
        PermissionPolicy::new(mode)
            .with_permission_rules(feature_config.permission_rules())
            .with_memory_allow_rules(&memory_dir),
        |policy, (name, required_permission)| {
            policy.with_tool_requirement(name, required_permission)
        },
    ))
}
