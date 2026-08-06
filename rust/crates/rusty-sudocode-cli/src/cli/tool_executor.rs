use std::cell::RefCell;
use std::io::{self, IsTerminal, Write};
use std::sync::{Arc, Mutex};

use runtime::{
    ContentBlock, PermissionMode, PermissionPolicy, QuestionField, QuestionKind, QuestionOption,
    QuestionPromptAnswer, QuestionPromptRequest, QuestionPrompter, ToolError, ToolExecutor,
};
use serde::Deserialize;
use tools::GlobalToolRegistry;

use super::format::format_tool_result;
use crate::render::{SpinnerRef, TerminalRenderer};
use crate::{AllowedToolSet, RuntimeMcpState};

// ---------------------------------------------------------------------------
// Thread-local side-channel for "clear context & execute plan" flow.
//
// When the user chooses option 1 ("Clear context & execute") in the
// ExitPlanMode confirmation dialog, the tool executor stores the plan text
// here. After `runtime.run_turn()` returns, `LiveCli::run_turn()` checks
// this value and, if set, clears the session and re-runs with the plan as
// the new prompt.
// ---------------------------------------------------------------------------
thread_local! {
    static PENDING_PLAN_EXECUTION: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Store a plan for the `run_turn` caller to pick up after the turn.
fn set_pending_plan_execution(plan: String) {
    PENDING_PLAN_EXECUTION.with(|cell| {
        *cell.borrow_mut() = Some(plan);
    });
}

/// Take the pending plan (if any), clearing the thread-local.
pub(crate) fn take_pending_plan_execution() -> Option<String> {
    PENDING_PLAN_EXECUTION.with(|cell| cell.borrow_mut().take())
}

/// Clear the pending plan without returning it.
pub(crate) fn clear_pending_plan_execution() {
    PENDING_PLAN_EXECUTION.with(|cell| {
        cell.borrow_mut().take();
    });
}

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

#[derive(Debug, Deserialize)]
pub(crate) struct ListMcpPromptsRequest {
    pub(crate) server: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct GetMcpPromptRequest {
    pub(crate) server: String,
    pub(crate) name: String,
    pub(crate) arguments: Option<serde_json::Value>,
}

pub(crate) struct CliToolExecutor {
    renderer: TerminalRenderer,
    emit_output: bool,
    is_repl: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    spinner: Option<SpinnerRef>,
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
            is_repl: false,
            allowed_tools,
            tool_registry,
            mcp_state,
            spinner: None,
            question_prompter: None,
            abort_signal: None,
        }
    }

    pub(crate) fn set_spinner(&mut self, ref_: SpinnerRef) {
        self.spinner = Some(ref_);
    }

    pub(crate) fn set_repl_mode(&mut self, is_repl: bool) {
        self.is_repl = is_repl;
    }

    pub(crate) fn set_question_prompter(&mut self, prompter: Box<dyn QuestionPrompter>) {
        self.question_prompter = Some(prompter);
    }

    /// Pause the spinner and clear its line before writing content.
    fn pause_spinner(&self) {
        if let Some(s) = &self.spinner {
            s.pause();
        }
    }

    /// Resume the spinner after content has been written.
    fn resume_spinner(&self) {
        if let Some(s) = &self.spinner {
            s.resume();
        }
    }

    /// Build a progress callback that prints MCP tool progress notifications
    /// to the terminal. Returns `None` when no spinner ref is configured.
    fn make_mcp_progress_callback(&self) -> Option<runtime::McpProgressCallback> {
        let spinner = self.spinner.clone()?;
        Some(Box::new(
            move |progress: runtime::McpProgressNotification| {
                let mut status = String::new();
                if let Some(total) = progress.total {
                    if total > 0.0 {
                        let pct = (progress.progress / total * 100.0).min(100.0);
                        status = format!(" ({pct:.0}%)");
                    }
                }
                let line = if let Some(msg) = &progress.message {
                    format!("  \x1b[2m\u{27f3} {msg}{status}\x1b[0m")
                } else {
                    format!(
                        "  \x1b[2m\u{27f3} progress: {:.0}{status}\x1b[0m",
                        progress.progress
                    )
                };
                spinner.suspend(|| {
                    let _ = writeln!(io::stdout(), "{line}");
                    let _ = io::stdout().flush();
                });
            },
        ))
    }

    fn make_bash_progress_callback(&self) -> Option<runtime::BashProgressCallback> {
        let spinner = self.spinner.clone()?;
        Some(Box::new(move |progress: runtime::BashProgress<'_>| {
            let trimmed = progress.output.trim_end();
            if trimmed.is_empty() {
                return;
            }
            let last_line = trimmed.lines().next_back().unwrap_or("");
            let bytes_display = if progress.total_bytes >= 1024 {
                format!("{:.1} KB", progress.total_bytes as f64 / 1024.0)
            } else {
                format!("{} B", progress.total_bytes)
            };
            let line = format!(
                "  \x1b[2m⟳ {last_line}  ({} lines, {bytes_display})\x1b[0m",
                progress.total_lines,
            );
            spinner.suspend(|| {
                let _ = writeln!(io::stdout(), "{line}");
                let _ = io::stdout().flush();
            });
        }))
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
            "ListMcpPromptsTool" => {
                let input: ListMcpPromptsRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                match input.server {
                    Some(server_name) => mcp_state.list_prompts_for_server(&server_name),
                    None => mcp_state.list_prompts_for_all_servers(),
                }
            }
            "GetMcpPromptTool" => {
                let input: GetMcpPromptRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                mcp_state.get_prompt(&input.server, &input.name, input.arguments)
            }
            _ => mcp_state.call_tool(tool_name, Some(value)),
        }
    }
}

/// Parse a model-supplied tool-call argument string into a JSON value.
///
/// A tool invoked with no arguments arrives as an empty string or the literal
/// `null`: the streaming openai-completions wire form for "no arguments"
/// accumulates an empty `partial_json`, and non-streaming providers can send
/// `null`. A plain `from_str` rejects both, which broke every no-argument tool
/// (`EnterPlanMode`, `ExitPlanMode`, …) with "invalid tool input JSON" before
/// the call ever reached dispatch. Normalize empty / `null` to an empty
/// argument object here — the single point where the CLI turns a tool-call
/// argument string into a value — so no-arg tools dispatch. Tools with required
/// fields still fail later with a clear "missing field"; genuinely malformed
/// JSON still errors here.
fn parse_tool_call_input(input: &str) -> Result<serde_json::Value, ToolError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(serde_json::Value::Null) => Ok(serde_json::Value::Object(serde_json::Map::new())),
        Ok(value) => Ok(value),
        Err(error) => Err(ToolError::new(format!("invalid tool input JSON: {error}"))),
    }
}

impl ToolExecutor for CliToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        self.execute_with_context(tool_name, input, &runtime::ToolDispatchContext::default())
    }

    fn execute_with_context(
        &mut self,
        tool_name: &str,
        input: &str,
        ctx: &runtime::ToolDispatchContext,
    ) -> Result<String, ToolError> {
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(tool_name))
        {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled by the current --allowedTools setting"
            )));
        }
        let value = parse_tool_call_input(input)?;
        if tool_name == "AskUserQuestion" && self.question_prompter.is_some() {
            return self.execute_ask_user_question(value);
        }
        // In REPL mode, intercept ExitPlanMode to show a confirmation
        // dialog. In one-shot mode, skip the dialog and let ExitPlanMode
        // execute normally — there is no interactive user to ask.
        if tool_name == "ExitPlanMode" && self.emit_output && self.is_repl {
            return self.handle_exit_plan_mode(&value, ctx);
        }
        // For bash tool calls, install a streaming progress callback so
        // the user sees output as it is produced rather than only after
        // the child process exits.
        if tool_name == "bash" && self.emit_output {
            if let Some(cb) = self.make_bash_progress_callback() {
                runtime::set_bash_progress_callback(cb);
            }
        }

        let is_mcp_tool = self.tool_registry.has_runtime_tool(tool_name);
        if is_mcp_tool && self.emit_output {
            if let Some(cb) = self.make_mcp_progress_callback() {
                runtime::set_mcp_progress_callback(cb);
            }
        }

        let result = if tool_name == "ToolSearch" {
            self.execute_search_tool(value)
        } else if is_mcp_tool {
            self.execute_runtime_tool(tool_name, value)
        } else {
            self.tool_registry
                .execute_with_abort_and_context(
                    tool_name,
                    &value,
                    self.abort_signal.as_ref(),
                    Some(ctx),
                )
                .map_err(ToolError::new)
        };

        // Ensure the thread-local is cleaned up regardless of the code
        // path that was taken (the callback is consumed inside
        // `execute_bash_with_abort`, but clear defensively).
        if tool_name == "bash" {
            runtime::clear_bash_progress_callback();
        }
        if is_mcp_tool {
            runtime::clear_mcp_progress_callback();
        }
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

    /// Intercept `ExitPlanMode` to present a confirmation dialog before
    /// transitioning out of plan mode. The user chooses between:
    ///   1. Clear context & execute the plan as a fresh prompt
    ///   2. Keep context & exit plan mode (current behavior)
    ///   3. Stay in plan mode and refine the plan
    fn handle_exit_plan_mode(
        &self,
        value: &serde_json::Value,
        ctx: &runtime::ToolDispatchContext,
    ) -> Result<String, ToolError> {
        // Extract the plan text from the assistant message that emitted this
        // tool call. This is the model's plan output.
        let plan_text = ctx
            .parent_assistant_message
            .as_ref()
            .map(|msg| {
                msg.blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();

        let plan_display = if plan_text.is_empty() {
            "(no plan text available)".to_string()
        } else {
            plan_text.clone()
        };

        // Non-interactive: default to option 2 (keep context & execute).
        if !io::stdin().is_terminal() {
            return self
                .tool_registry
                .execute_with_abort_and_context(
                    "ExitPlanMode",
                    value,
                    self.abort_signal.as_ref(),
                    Some(ctx),
                )
                .map_err(ToolError::new);
        }

        // Show plan summary and prompt the user.
        let print_line = |line: &str| {
            let _ = writeln!(io::stdout(), "{line}");
        };

        self.pause_spinner();

        print_line("");
        print_line("\x1b[1m\u{1f4cb} Plan ready for review:\x1b[0m");
        print_line("");

        let lines: Vec<&str> = plan_display.lines().collect();
        let display_limit = 20;
        for line in lines.iter().take(display_limit) {
            print_line(&format!("  \x1b[2m{line}\x1b[0m"));
        }
        if lines.len() > display_limit {
            print_line(&format!(
                "  \x1b[2m... ({} more lines)\x1b[0m",
                lines.len() - display_limit
            ));
        }

        print_line("");
        print_line("\x1b[1mChoose an action:\x1b[0m");
        print_line("  \x1b[36m[1]\x1b[0m Clear context & execute plan");
        print_line("  \x1b[36m[2]\x1b[0m Keep context & execute");
        print_line("  \x1b[36m[3]\x1b[0m Keep planning (provide feedback)");
        print_line("");

        // Read the choice from the user.
        print!("\x1b[1mYour choice (1/2/3): \x1b[0m");
        io::stdout()
            .flush()
            .map_err(|e| ToolError::new(e.to_string()))?;
        let mut choice = String::new();
        io::stdin()
            .read_line(&mut choice)
            .map_err(|e| ToolError::new(e.to_string()))?;
        let choice = choice.trim();

        match choice {
            "1" => {
                // Execute ExitPlanMode to restore the previous permission mode.
                let result = self
                    .tool_registry
                    .execute_with_abort_and_context(
                        "ExitPlanMode",
                        &value,
                        self.abort_signal.as_ref(),
                        Some(ctx),
                    )
                    .map_err(ToolError::new)?;

                // Store the plan text for run_turn to pick up.
                let plan_for_execution = if plan_text.is_empty() {
                    // Fallback: use the full plan display.
                    plan_display
                } else {
                    plan_text
                };
                set_pending_plan_execution(plan_for_execution);

                print_line("\x1b[32m\u{2714} Plan confirmed. Will clear context and execute...\x1b[0m");
                self.resume_spinner();
                Ok(result)
            }
            "2" => {
                // Normal ExitPlanMode execution — keep context.
                let result = self
                    .tool_registry
                    .execute_with_abort_and_context(
                        "ExitPlanMode",
                        &value,
                        self.abort_signal.as_ref(),
                        Some(ctx),
                    )
                    .map_err(ToolError::new)?;

                print_line("\x1b[32m\u{2714} Exiting plan mode, keeping context.\x1b[0m");
                self.resume_spinner();
                Ok(result)
            }
            _ => {
                // Choice 3 or any other input: stay in plan mode.
                print_line("\x1b[33m\u{21a9} Staying in plan mode. Provide feedback to refine the plan.\x1b[0m");
                self.resume_spinner();
                Err(ToolError::new(
                    "User chose to continue planning. Please ask the user for feedback and refine the plan based on their input.",
                ))
            }
        }
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

#[cfg(test)]
mod tests {
    use super::parse_tool_call_input;
    use serde_json::json;

    #[test]
    fn empty_or_null_arguments_normalize_to_empty_object() {
        // No-argument tool calls stream an empty arguments string (the
        // openai-completions wire form) or the literal `null`. Both must
        // dispatch as `{}` so no-arg tools (EnterPlanMode, ExitPlanMode, …)
        // deserialize into their empty input structs instead of being rejected
        // with "invalid tool input JSON" before dispatch.
        for raw in ["", "   ", "\n\t", "null", "  null  "] {
            assert_eq!(
                parse_tool_call_input(raw).expect("no-arg input must parse"),
                json!({}),
                "input {raw:?} should normalize to an empty object",
            );
        }
    }

    #[test]
    fn well_formed_arguments_pass_through_unchanged() {
        assert_eq!(
            parse_tool_call_input(r#"{"path":"a.txt","limit":5}"#).unwrap(),
            json!({ "path": "a.txt", "limit": 5 }),
        );
    }

    #[test]
    fn genuinely_malformed_json_still_errors() {
        assert!(parse_tool_call_input("{ not json").is_err());
        assert!(parse_tool_call_input("[1, 2").is_err());
    }
}
