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
use crate::render::{ansi_bold_fg, ansi_fg, theme, SpinnerRef, TerminalRenderer, BOLD, DIM, RESET};
use crate::repl_ui::OutputSender;
use crate::{AllowedToolSet, RuntimeMcpState};

// ---------------------------------------------------------------------------
// Global side-channel for the "clear context & execute plan" flow.
//
// When the user chooses option 1 ("Clear context & execute") in the
// ExitPlanMode confirmation dialog, the tool executor (running on the ENGINE
// thread, deep in tool dispatch) stores the plan text here. After the turn,
// `LiveCli::run_turn()` (the RENDERER thread) reads it and, if set, clears the
// session and re-runs with the plan as the new prompt. It is a *global* Mutex
// (not a thread-local) precisely because it must cross the engine↔renderer
// thread boundary of the seam — and survive tools that dispatch on a worker
// thread (parallel read-only tools). scode runs one turn at a time, so there is
// no cross-turn race.
// ---------------------------------------------------------------------------
static PENDING_PLAN_EXECUTION: Mutex<Option<String>> = Mutex::new(None);

fn plan_execution_slot() -> std::sync::MutexGuard<'static, Option<String>> {
    PENDING_PLAN_EXECUTION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Store a plan for `LiveCli::run_turn` to pick up after the turn.
fn set_pending_plan_execution(plan: String) {
    *plan_execution_slot() = Some(plan);
}

/// Take the pending plan (if any), clearing the slot.
pub(crate) fn take_pending_plan_execution() -> Option<String> {
    plan_execution_slot().take()
}

/// Clear the pending plan without returning it.
pub(crate) fn clear_pending_plan_execution() {
    *plan_execution_slot() = None;
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
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    // Single-threaded interior mutability: `execute` is `&self` (so a
    // concurrency-safe batch can share the dispatcher), but the interactive
    // AskUserQuestion prompter needs `&mut` to drive a prompt. AskUserQuestion
    // is non-concurrency-safe (runs serial), so a `RefCell` (not a `Mutex`) is
    // correct — the CLI turn loop is single-threaded.
    question_prompter: RefCell<Option<Box<dyn QuestionPrompter>>>,
    abort_signal: Option<runtime::HookAbortSignal>,
    /// Optional UI command sender for ContextSlot updates (Task* → UI).
    ui_sender: Option<crate::repl_ui::UiCommandSender>,
    /// Optional nexus A2A send capability. When set (nexus-A2A configured),
    /// `send_message` writes to the peer's `/agents/<to>/chat-with-me`
    /// DT_STREAM over gRPC (the node stamps `from`) via the SAME shared
    /// `handle_send_message` the co-host uses — only the transport differs.
    /// Held here like the other per-session capabilities.
    mailbox_sender: Option<runtime::spawn_task::MailboxSender>,
}

impl CliToolExecutor {
    pub(crate) fn new(
        allowed_tools: Option<AllowedToolSet>,
        tool_registry: GlobalToolRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            renderer: TerminalRenderer::new(),
            allowed_tools,
            tool_registry,
            mcp_state,
            question_prompter: RefCell::new(None),
            abort_signal: None,
            ui_sender: None,
            mailbox_sender: None,
        }
    }

    /// Enable nexus A2A: `send_message` now routes to the peer's replicated
    /// DT_STREAM inbox over gRPC (via the shared `handle_send_message`).
    /// Set at startup only when nexus-A2A is configured; absent it,
    /// `send_message` is not advertised to the model.
    pub(crate) fn set_mailbox_sender(&mut self, sender: runtime::spawn_task::MailboxSender) {
        self.mailbox_sender = Some(sender);
    }

    pub(crate) fn set_ui_sender(&mut self, sender: crate::repl_ui::UiCommandSender) {
        self.ui_sender = Some(sender);
    }

    pub(crate) fn set_question_prompter(&mut self, prompter: Box<dyn QuestionPrompter>) {
        *self.question_prompter.get_mut() = Some(prompter);
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
    async fn execute(&self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        self.execute_with_context(tool_name, input, &runtime::ToolDispatchContext::default())
            .await
    }

    async fn execute_with_context(
        &self,
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
        // nexus A2A: when configured, `send_message` routes to the peer's
        // replicated DT_STREAM inbox over gRPC — the SAME shared handler the
        // co-host's `ManagedToolExecutor` uses (only the transport differs).
        // Absent config the tool is never advertised, so this is unreachable.
        if tool_name == "send_message" {
            if let Some(sender) = &self.mailbox_sender {
                return runtime::spawn_task::handle_send_message(sender, input)
                    .map_err(ToolError::new);
            }
        }
        let value = parse_tool_call_input(input)?;
        if tool_name == "AskUserQuestion" && self.question_prompter.borrow().is_some() {
            return self.execute_ask_user_question(value);
        }
        // Intercept ExitPlanMode to ask the user (across the seam) how to
        // proceed. The confirmation crosses via `question_prompter` — the pump's
        // QuestionAdapter emits a QuestionRequest the renderer answers — so it
        // works on every renderer (REPL dialog, iocraft, ACP client) and on
        // Windows (no raw stdin read_line). `handle_exit_plan_mode` itself falls
        // back to "execute normally" for non-interactive / no-prompter contexts.
        if tool_name == "ExitPlanMode" && self.question_prompter.borrow().is_some() {
            return self.handle_exit_plan_mode(&value, ctx);
        }
        // Live bash/MCP progress crosses the seam: when the renderer supplied a
        // progress sink (`ctx.progress_sink`), forward STRUCTURED progress to it
        // and the renderer formats + draws it (EngineEvent::ToolProgress). The
        // executor never writes progress to the terminal itself.
        if tool_name == "bash" {
            if let Some(sink) = ctx.progress_sink.clone() {
                runtime::set_bash_progress_callback(bash_progress_forward(sink));
            }
        }

        let is_mcp_tool = self.tool_registry.has_runtime_tool(tool_name);
        if is_mcp_tool {
            if let Some(sink) = ctx.progress_sink.clone() {
                runtime::set_mcp_progress_callback(mcp_progress_forward(sink));
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
        // After any Task* mutation succeeds, push the full task list
        // to the ContextSlot so the UI renders live progress.
        if matches!(
            tool_name,
            "TaskCreate" | "TaskUpdate" | "TaskList" | "TaskStop"
        ) {
            if let (Ok(_), Some(ref ui)) = (&result, &self.ui_sender) {
                let tasks = tools::global_task_list();
                ui.update_context(tasks);
            }
        }
        // Tool results reach the renderer via RuntimeObserver::on_tool_result
        // -> EngineEvent::ToolResult -> EngineEventRenderer (the seam). The
        // executor stays renderer-agnostic and never writes to the terminal.
        result
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
    fn execute_ask_user_question(&self, value: serde_json::Value) -> Result<String, ToolError> {
        let input: AskUserQuestionCliInput = serde_json::from_value(value)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;

        let mut prompter_guard = self.question_prompter.borrow_mut();
        let Some(prompter) = prompter_guard.as_mut() else {
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

        // Execute ExitPlanMode normally (restores the previous permission mode).
        let execute_normally = || {
            self.tool_registry
                .execute_with_abort_and_context(
                    "ExitPlanMode",
                    value,
                    self.abort_signal.as_ref(),
                    Some(ctx),
                )
                .map_err(ToolError::new)
        };

        // Non-interactive: no user to ask — keep context & execute.
        if !io::stdin().is_terminal() {
            return execute_normally();
        }

        // Truncate the plan to a summary for the confirmation dialog.
        let plan_summary = {
            let lines: Vec<&str> = plan_display.lines().collect();
            let display_limit = 20;
            let mut summary = lines
                .iter()
                .take(display_limit)
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            if lines.len() > display_limit {
                summary.push_str(&format!(
                    "\n... ({} more lines)",
                    lines.len() - display_limit
                ));
            }
            summary
        };

        // Ask the user how to proceed — ACROSS THE SEAM. The pump's
        // QuestionAdapter turns this into a QuestionRequest the renderer answers
        // (the REPL / iocraft / ACP each draw the dialog + read the choice their
        // own way), so it works on Windows too (no raw stdin read_line here).
        let request = QuestionPromptRequest {
            title: Some("Choose an action".to_string()),
            description: Some(plan_summary),
            fields: vec![QuestionField {
                id: "action".to_string(),
                prompt: "Choose an action".to_string(),
                kind: QuestionKind::SingleSelect,
                required: true,
                allow_custom_input: false,
                custom_input_hint: None,
                options: vec![
                    QuestionOption {
                        label: "Clear context & execute plan".to_string(),
                        value: "1".to_string(),
                        description: None,
                        recommended: false,
                    },
                    QuestionOption {
                        label: "Keep context & execute".to_string(),
                        value: "2".to_string(),
                        description: None,
                        recommended: true,
                    },
                    QuestionOption {
                        label: "Keep planning (provide feedback)".to_string(),
                        value: "3".to_string(),
                        description: None,
                        recommended: false,
                    },
                ],
            }],
        };

        // Compute the choice, dropping the prompter borrow before we act on it.
        let choice = {
            let mut guard = self.question_prompter.borrow_mut();
            let Some(prompter) = guard.as_mut() else {
                return execute_normally();
            };
            let answers = prompter.ask(&request).map_err(ToolError::new)?;
            answers
                .first()
                .map_or_else(|| "2".to_string(), |answer| answer.value.clone())
        };

        match choice.as_str() {
            "1" => {
                let result = execute_normally()?;
                // Store the plan for LiveCli::run_turn to pick up: it clears the
                // session and re-runs with the plan as the new prompt.
                let plan_for_execution = if plan_text.is_empty() {
                    plan_display
                } else {
                    plan_text
                };
                set_pending_plan_execution(plan_for_execution);
                Ok(result)
            }
            "3" => Err(ToolError::new(
                "User chose to continue planning. Please ask the user for feedback and refine the plan based on their input.",
            )),
            // "2" or any unrecognized answer: keep context & execute.
            _ => execute_normally(),
        }
    }
}

/// Build a `bash` progress callback that forwards STRUCTURED progress to a seam
/// [`runtime::ProgressSink`] instead of drawing it — the renderer above the seam
/// formats and prints it. Preserves the empty-output filter of the legacy
/// terminal callback so live-progress output stays byte-identical.
fn bash_progress_forward(sink: runtime::ProgressSink) -> runtime::BashProgressCallback {
    Box::new(move |progress: runtime::BashProgress<'_>| {
        let trimmed = progress.output.trim_end();
        if trimmed.is_empty() {
            return;
        }
        let last_line = trimmed.lines().next_back().unwrap_or("").to_string();
        sink.emit(runtime::ToolProgressEvent::Bash {
            last_line,
            total_lines: progress.total_lines,
            total_bytes: progress.total_bytes,
        });
    })
}

/// Build an MCP progress callback that forwards STRUCTURED progress to a seam
/// [`runtime::ProgressSink`].
fn mcp_progress_forward(sink: runtime::ProgressSink) -> runtime::McpProgressCallback {
    Box::new(move |progress: runtime::McpProgressNotification| {
        sink.emit(runtime::ToolProgressEvent::Mcp {
            message: progress.message,
            progress: progress.progress,
            total: progress.total,
        });
    })
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
