//! ACP server implementation using the official `agent-client-protocol` SDK.
//!
//! This module provides an SDK-based ACP server with full ACP 1.0 compliance
//! including capabilities declaration, session cancel, permission-mode switching,
//! model switching, image input, and permission-prompt bridging (elicitation).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent_client_protocol::role::acp::{Agent, Client};
// NOTE: `ConnectTo` and `ConnectionTo` are different SDK concepts:
//   - `ConnectTo<R>`:    trait for wiring up a transport (Stdio, Lines, etc.)
//   - `ConnectionTo<R>`: runtime handle passed to handlers for sending messages
use crate::conversation::RuntimeObserver;
use crate::hooks::HookAbortSignal;
use crate::permissions::{
    PermissionMode, PermissionPromptDecision, PermissionPrompter, PermissionRequest,
};
use agent_client_protocol::{
    on_receive_dispatch, on_receive_notification, on_receive_request, ConnectTo, ConnectionTo,
    Dispatch, Error, JsonRpcRequest, JsonRpcResponse, Responder,
};
use agent_client_protocol_schema::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse, ContentBlock,
    ContentChunk, Implementation, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionId, PermissionOptionKind,
    PromptCapabilities, PromptRequest, PromptResponse, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SessionCapabilities,
    SessionCloseCapabilities, SessionInfo, SessionNotification, SessionUpdate,
    SetSessionModelRequest, SetSessionModelResponse, StopReason, TextContent, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, Usage,
};

/// Error type returned by ACP agent implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpError {
    InvalidParams(String),
    Internal(String),
}

impl AcpError {
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::InvalidParams(message.into())
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidParams(message) | Self::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for AcpError {}

/// Configuration for the SDK-based ACP server.
#[derive(Debug, Clone)]
pub struct SdkAcpConfig {
    pub agent_version: String,
    pub model: String,
    pub model_flag_raw: Option<String>,
    pub permission_mode_override: Option<PermissionMode>,
    pub reasoning_effort: Option<String>,
}

// ---------------------------------------------------------------------------
// Custom extension: session/setPermissionMode (not in ACP SDK schema)
// ---------------------------------------------------------------------------

/// Request to change the permission mode for a session.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcRequest)]
#[request(method = "session/setPermissionMode", response = SetPermissionModeResponse)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SetPermissionModeRequest {
    pub session_id: String,
    pub permission_mode: String,
}

/// Response to a permission mode change.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, JsonRpcResponse)]
pub(crate) struct SetPermissionModeResponse {}

/// Callback trait that the CLI crate implements to provide session
/// construction and prompt execution, keeping runtime/provider deps out of
/// this crate.
pub trait SdkAcpDelegate: Send + 'static {
    /// Create a new session for the given working directory, returning
    /// `(session_id, cwd, abort_signal)` on success.
    fn new_session(&mut self, cwd: PathBuf)
        -> Result<(String, PathBuf, HookAbortSignal), AcpError>;

    /// Run a prompt turn. The implementation should call observer methods
    /// to stream session updates.
    fn run_prompt(
        &mut self,
        session_id: &str,
        prompt: String,
        observer: &mut SdkSessionObserver,
    ) -> Result<(StopReason, Option<PromptUsage>), AcpError>;

    /// Run a prompt with permission prompting bridged to the ACP client.
    fn run_prompt_with_prompter(
        &mut self,
        session_id: &str,
        prompt: String,
        observer: &mut SdkSessionObserver,
        prompter: &mut dyn PermissionPrompter,
    ) -> Result<(StopReason, Option<PromptUsage>), AcpError>;

    /// Handle a slash command, returning text output.
    fn handle_slash_command(
        &mut self,
        session_id: &str,
        input: &str,
        observer: &mut SdkSessionObserver,
    ) -> Result<(), AcpError>;

    /// List active session IDs with their cwds.
    fn list_sessions(&self) -> Vec<(String, PathBuf)>;

    /// Close (drop) a session by ID. Returns true if it existed.
    fn close_session(&mut self, session_id: &str) -> bool;

    /// Switch the model for a session. Returns a human-readable report.
    fn set_model(&mut self, session_id: &str, model_id: &str) -> Result<String, AcpError>;

    /// Return the current model ID and available models.
    fn get_model_info(&self) -> (String, Vec<String>);

    /// Change the permission mode for a session.
    fn set_permission_mode(
        &mut self,
        session_id: &str,
        mode: PermissionMode,
    ) -> Result<(), AcpError>;

    /// Push image content blocks into a session before running a prompt.
    fn push_images(
        &mut self,
        session_id: &str,
        images: &[(String, String)],
    ) -> Result<(), AcpError>;

    /// Load an existing persisted session by its ID and working directory,
    /// returning `(session_id, cwd, abort_signal)` on success.
    fn load_session(
        &mut self,
        session_id: &str,
        cwd: PathBuf,
    ) -> Result<(String, PathBuf, HookAbortSignal), AcpError>;
}

/// Observer that streams session update notifications to the ACP client in
/// real time via a channel. Implements [`RuntimeObserver`] so existing
/// `run_turn()` machinery can drive it.
pub struct SdkSessionObserver {
    session_id: String,
    tx: tokio::sync::mpsc::UnboundedSender<SessionNotification>,
}

impl SdkSessionObserver {
    /// Create a new observer that sends notifications through `tx`.
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        tx: tokio::sync::mpsc::UnboundedSender<SessionNotification>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            tx,
        }
    }

    fn push(&mut self, update: SessionUpdate) {
        let _ = self
            .tx
            .send(SessionNotification::new(self.session_id.clone(), update));
    }
}

impl RuntimeObserver for SdkSessionObserver {
    fn on_text_delta(&mut self, delta: &str) {
        self.push(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(delta)),
        )));
    }

    fn on_tool_use(&mut self, id: &str, name: &str, input: &str) {
        let id_owned = id.to_owned();
        let name_owned = name.to_owned();
        let raw_input = serde_json::from_str(input)
            .unwrap_or_else(|_| serde_json::Value::String(input.to_owned()));
        self.push(SessionUpdate::ToolCall(
            ToolCall::new(id_owned, name_owned)
                .kind(ToolKind::Other)
                .status(ToolCallStatus::InProgress)
                .raw_input(raw_input),
        ));
    }

    fn on_tool_result(
        &mut self,
        tool_use_id: &str,
        _tool_name: &str,
        output: &str,
        is_error: bool,
    ) {
        let id_owned = tool_use_id.to_owned();
        let raw_output = serde_json::from_str(output)
            .unwrap_or_else(|_| serde_json::Value::String(output.to_owned()));
        let status = if is_error {
            ToolCallStatus::Failed
        } else {
            ToolCallStatus::Completed
        };
        self.push(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id_owned,
            ToolCallUpdateFields::new()
                .status(status)
                .raw_output(raw_output),
        )));
    }
}

/// Sniff the MIME type of a base64-encoded image from its leading bytes.
///
/// Inspects the first few characters of the base64 data to detect the format.
/// Falls back to `image/png` when the prefix is unrecognised.
pub(crate) fn sniff_image_mime(base64_data: &str) -> &'static str {
    if base64_data.starts_with("iVBOR") {
        "image/png"
    } else if base64_data.starts_with("/9j/") {
        "image/jpeg"
    } else if base64_data.starts_with("R0lGO") {
        "image/gif"
    } else if base64_data.starts_with("UklGR") {
        "image/webp"
    } else {
        "image/png"
    }
}

/// Extract plain text from a slice of ACP `ContentBlock`s. Image blocks are
/// tracked separately and returned as `(text, images)`.
pub(crate) fn extract_content_from_blocks(
    blocks: &[ContentBlock],
) -> Result<(String, Vec<(String, String)>), AcpError> {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(tc) => {
                let t = tc.text.trim();
                if !t.is_empty() {
                    texts.push(t.to_owned());
                }
            }
            ContentBlock::Image(ic) => {
                let mime = if ic.mime_type.is_empty() {
                    sniff_image_mime(&ic.data).to_owned()
                } else {
                    ic.mime_type.clone()
                };
                images.push((ic.data.clone(), mime));
            }
            _ => {}
        }
    }
    if texts.is_empty() && images.is_empty() {
        return Err(AcpError::invalid_params(
            "prompt must include at least one non-empty text or image content block",
        ));
    }
    Ok((texts.join("\n"), images))
}

/// Re-export `StopReason` so the CLI crate doesn't need a direct dep on
/// the schema crate.
pub use agent_client_protocol_schema::StopReason as AcpStopReason;

/// Token usage data returned by a prompt turn.
#[derive(Debug, Clone, Default)]
pub struct PromptUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

/// Thread-safe handle to a delegate, shared across async handlers.
pub type SharedDelegate = Arc<Mutex<Box<dyn SdkAcpDelegate>>>;

/// Separate registry of abort signals so that `session/cancel` can fire
/// without contending on the main delegate mutex.
pub type AbortRegistry = Arc<Mutex<HashMap<String, HookAbortSignal>>>;

/// Create a new empty abort registry. Share this across connections so that
/// cancel notifications on a reconnected transport can still reach sessions
/// created on a previous connection.
#[must_use]
pub fn new_abort_registry() -> AbortRegistry {
    Arc::new(Mutex::new(HashMap::new()))
}

/// A permission prompter that bridges to the ACP client over channels.
///
/// From inside the blocking `spawn_blocking` context, `decide()` sends
/// the permission request to an async handler which forwards it to the
/// ACP client, then blocks waiting for the response.
struct AcpPermissionBridge {
    tx: tokio::sync::mpsc::UnboundedSender<(
        PermissionRequest,
        tokio::sync::oneshot::Sender<PermissionPromptDecision>,
    )>,
}

impl PermissionPrompter for AcpPermissionBridge {
    fn decide(&mut self, request: &PermissionRequest) -> PermissionPromptDecision {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        if self.tx.send((request.clone(), response_tx)).is_err() {
            return PermissionPromptDecision::Deny {
                reason: "permission bridge closed".to_string(),
            };
        }
        response_rx
            .blocking_recv()
            .unwrap_or(PermissionPromptDecision::Deny {
                reason: "permission response channel closed".to_string(),
            })
    }
}

/// Build an ACP `RequestPermissionRequest` from a runtime `PermissionRequest`.
fn build_acp_permission_request(
    session_id: String,
    request: &PermissionRequest,
) -> RequestPermissionRequest {
    let tool_call = ToolCallUpdate::new(
        format!("perm-{}", uuid_v4()),
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::InProgress)
            .raw_input(serde_json::Value::String(request.input.clone())),
    );

    let options = vec![
        PermissionOption::new(
            PermissionOptionId::new("allow_once"),
            "Allow Once",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::new("allow_always"),
            "Allow Always",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            PermissionOptionId::new("reject_once"),
            "Reject Once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::new("reject_always"),
            "Reject Always",
            PermissionOptionKind::RejectAlways,
        ),
    ];

    RequestPermissionRequest::new(session_id, tool_call, options)
}

/// Map an ACP permission response to a `PermissionPromptDecision`.
fn map_permission_response(response: RequestPermissionResponse) -> PermissionPromptDecision {
    match response.outcome {
        RequestPermissionOutcome::Selected(selected) => {
            let id_str: &str = &selected.option_id.0;
            if id_str.starts_with("allow") {
                PermissionPromptDecision::Allow
            } else {
                PermissionPromptDecision::Deny {
                    reason: format!("user selected: {id_str}"),
                }
            }
        }
        RequestPermissionOutcome::Cancelled | _ => PermissionPromptDecision::Deny {
            reason: "user cancelled permission prompt".to_string(),
        },
    }
}

/// Generate a pseudo-random UUID v4 string without pulling in the `uuid` crate.
fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:032x}")
}

// ---------------------------------------------------------------------------
// Shared handler chain
// ---------------------------------------------------------------------------

/// Run the ACP agent handler chain on an arbitrary transport.
///
/// This is the shared core used by both the stdio server and the WebSocket
/// server. The transport must implement `ConnectTo<Agent>` (e.g. `Stdio` or
/// `Lines`).
#[allow(clippy::too_many_lines)]
pub(crate) async fn run_acp_on_transport(
    config: &SdkAcpConfig,
    delegate: SharedDelegate,
    abort_registry: AbortRegistry,
    transport: impl ConnectTo<Agent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let agent_version = config.agent_version.clone();

    Agent
        .builder()
        .name("scode")
        // --- initialize ---
        .on_receive_request(
            {
                let version = agent_version.clone();
                async move |req: InitializeRequest,
                            responder: Responder<InitializeResponse>,
                            _cx: ConnectionTo<Client>| {
                    let resp = InitializeResponse::new(req.protocol_version)
                        .agent_info(Implementation::new("scode", &version))
                        .agent_capabilities(
                            AgentCapabilities::new()
                                .prompt_capabilities(PromptCapabilities::new().image(true))
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .close(SessionCloseCapabilities::new()),
                                ),
                        );
                    responder.respond(resp)?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/new ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                let abort_registry = Arc::clone(&abort_registry);
                async move |req: NewSessionRequest,
                            responder: Responder<NewSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    let d = Arc::clone(&delegate);
                    let registry = Arc::clone(&abort_registry);
                    cx.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            d.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .new_session(req.cwd)
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));

                        match result {
                            Ok((session_id, _cwd, signal)) => {
                                registry
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .insert(session_id.clone(), signal);
                                responder.respond(NewSessionResponse::new(session_id))?;
                            }
                            Err(e) => {
                                responder.respond_with_error(acp_error_to_sdk(&e))?;
                            }
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/prompt (with permission-prompt bridging) ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                async move |req: PromptRequest,
                            responder: Responder<PromptResponse>,
                            cx: ConnectionTo<Client>| {
                    let (prompt_text, images) = match extract_content_from_blocks(&req.prompt) {
                        Ok(r) => r,
                        Err(e) => {
                            responder.respond_with_error(acp_error_to_sdk(&e))?;
                            return Ok(());
                        }
                    };
                    // Text is required (images alone aren't enough to drive a turn).
                    if prompt_text.is_empty() {
                        responder.respond_with_error(acp_error_to_sdk(
                            &AcpError::invalid_params(
                                "prompt must include at least one non-empty text content block",
                            ),
                        ))?;
                        return Ok(());
                    }

                    let d = Arc::clone(&delegate);
                    let sid = req.session_id.to_string();
                    let cx_inner = cx.clone();
                    let cx_perm = cx.clone();
                    cx.spawn(async move {
                        // Set up permission-prompt bridge channels.
                        let (bridge_tx, mut bridge_rx) = tokio::sync::mpsc::unbounded_channel::<(
                            PermissionRequest,
                            tokio::sync::oneshot::Sender<PermissionPromptDecision>,
                        )>();

                        // Set up notification streaming channel.
                        let (notif_tx, mut notif_rx) =
                            tokio::sync::mpsc::unbounded_channel::<SessionNotification>();

                        let sid_for_blocking = sid.clone();
                        let sid_for_perm = sid.clone();
                        let blocking_handle = tokio::task::spawn_blocking(move || {
                            let mut observer = SdkSessionObserver::new(&sid_for_blocking, notif_tx);
                            let mut bridge = AcpPermissionBridge { tx: bridge_tx };
                            let mut delegate =
                                d.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

                            // Push image content blocks into the session before
                            // running the prompt so the API client includes them.
                            if !images.is_empty() {
                                let _ = delegate.push_images(&sid_for_blocking, &images);
                            }

                            let stop = if prompt_text.starts_with('/') {
                                delegate
                                    .handle_slash_command(
                                        &sid_for_blocking,
                                        &prompt_text,
                                        &mut observer,
                                    )
                                    .map(|()| (StopReason::EndTurn, None))
                            } else {
                                delegate.run_prompt_with_prompter(
                                    &sid_for_blocking,
                                    prompt_text,
                                    &mut observer,
                                    &mut bridge,
                                )
                            };
                            stop.unwrap_or((StopReason::EndTurn, None))
                        });

                        // Concurrently serve permission requests and stream
                        // notifications from the blocking thread while waiting
                        // for it to finish.
                        let mut blocking_handle = blocking_handle;
                        let mut notif_rx_open = true;
                        let result = loop {
                            tokio::select! {
                                biased;
                                notif = notif_rx.recv(), if notif_rx_open => {
                                    if let Some(n) = notif {
                                        let _ = cx_inner.send_notification(n);
                                    } else {
                                        // Sender dropped — stop polling this channel.
                                        notif_rx_open = false;
                                    }
                                }
                                perm = bridge_rx.recv() => {
                                    if let Some((perm_req, response_tx)) = perm {
                                        let acp_req = build_acp_permission_request(
                                            sid_for_perm.clone(),
                                            &perm_req,
                                        );
                                        let decision = match cx_perm
                                            .send_request(acp_req)
                                            .block_task()
                                            .await
                                        {
                                            Ok(resp) => map_permission_response(resp),
                                            Err(_) => PermissionPromptDecision::Deny {
                                                reason: "ACP permission request failed"
                                                    .to_string(),
                                            },
                                        };
                                        let _ = response_tx.send(decision);
                                    } else {
                                        // Channel closed — blocking task dropped the sender.
                                        // Await the result directly to avoid a busy loop
                                        // (biased select would keep picking this branch).
                                        break blocking_handle.await
                                            .unwrap_or((StopReason::EndTurn, None));
                                    }
                                }
                                done = &mut blocking_handle => {
                                    break done.unwrap_or((StopReason::EndTurn, None));
                                }
                            }
                        };

                        // Drain any residual notifications that were buffered
                        // before the blocking task returned.
                        while let Ok(n) = notif_rx.try_recv() {
                            let _ = cx_inner.send_notification(n);
                        }

                        let (stop_reason, prompt_usage) = result;

                        let mut response = PromptResponse::new(stop_reason);
                        if let Some(u) = prompt_usage {
                            response = response.usage(
                                Usage::new(u.total_tokens, u.input_tokens, u.output_tokens)
                                    .cached_read_tokens(u.cache_read_tokens)
                                    .cached_write_tokens(u.cache_write_tokens),
                            );
                        }
                        responder.respond(response)?;
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/cancel (notification) ---
        .on_receive_notification(
            {
                let abort_registry = Arc::clone(&abort_registry);
                async move |notif: CancelNotification, _cx: ConnectionTo<Client>| {
                    let sid = notif.session_id.to_string();
                    let signal = abort_registry
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .get(&sid)
                        .cloned();
                    if let Some(signal) = signal {
                        signal.abort();
                    }
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        // --- session/close ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                let abort_registry = Arc::clone(&abort_registry);
                async move |req: CloseSessionRequest,
                            responder: Responder<CloseSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    let d = Arc::clone(&delegate);
                    let registry = Arc::clone(&abort_registry);
                    let sid = req.session_id.to_string();
                    cx.spawn(async move {
                        let sid_clone = sid.clone();
                        tokio::task::spawn_blocking(move || {
                            d.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .close_session(&sid_clone);
                        })
                        .await
                        .ok();
                        registry
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .remove(&sid);
                        responder.respond(CloseSessionResponse::new())?;
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/list ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                async move |_req: ListSessionsRequest,
                            responder: Responder<ListSessionsResponse>,
                            cx: ConnectionTo<Client>| {
                    let d = Arc::clone(&delegate);
                    cx.spawn(async move {
                        let infos = tokio::task::spawn_blocking(move || {
                            d.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .list_sessions()
                                .into_iter()
                                .map(|(id, cwd)| SessionInfo::new(id, cwd))
                                .collect::<Vec<_>>()
                        })
                        .await
                        .unwrap_or_default();

                        responder.respond(ListSessionsResponse::new(infos))?;
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/setModel (unstable) ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                async move |req: SetSessionModelRequest,
                            responder: Responder<SetSessionModelResponse>,
                            cx: ConnectionTo<Client>| {
                    let d = Arc::clone(&delegate);
                    let sid = req.session_id.to_string();
                    let model_id: String = req.model_id.0.to_string();
                    cx.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            d.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .set_model(&sid, &model_id)
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));

                        match result {
                            Ok(_report) => {
                                responder.respond(SetSessionModelResponse::new())?;
                            }
                            Err(e) => {
                                responder.respond_with_error(acp_error_to_sdk(&e))?;
                            }
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/load ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                let abort_registry = Arc::clone(&abort_registry);
                async move |req: LoadSessionRequest,
                            responder: Responder<LoadSessionResponse>,
                            cx: ConnectionTo<Client>| {
                    let d = Arc::clone(&delegate);
                    let registry = Arc::clone(&abort_registry);
                    let sid = req.session_id.to_string();
                    let cwd = req.cwd;
                    cx.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            d.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .load_session(&sid, cwd)
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));

                        match result {
                            Ok((session_id, _cwd, signal)) => {
                                registry
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .insert(session_id, signal);
                                responder.respond(LoadSessionResponse::new())?;
                            }
                            Err(e) => {
                                responder.respond_with_error(acp_error_to_sdk(&e))?;
                            }
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- session/setPermissionMode (custom extension, not in SDK schema) ---
        .on_receive_request(
            {
                let delegate = Arc::clone(&delegate);
                async move |req: SetPermissionModeRequest,
                            responder: Responder<SetPermissionModeResponse>,
                            cx: ConnectionTo<Client>| {
                    let d = Arc::clone(&delegate);
                    cx.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            let mode = match req.permission_mode.as_str() {
                                "read-only" => Ok(PermissionMode::ReadOnly),
                                "workspace-write" => Ok(PermissionMode::WorkspaceWrite),
                                "danger-full-access" => Ok(PermissionMode::DangerFullAccess),
                                "prompt" => Ok(PermissionMode::Prompt),
                                "allow" => Ok(PermissionMode::Allow),
                                other => Err(AcpError::invalid_params(format!(
                                    "unknown permission mode: {other}"
                                ))),
                            };
                            match mode {
                                Ok(m) => d
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .set_permission_mode(&req.session_id, m),
                                Err(e) => Err(e),
                            }
                        })
                        .await
                        .unwrap_or_else(|e| Err(AcpError::internal(e.to_string())));
                        match result {
                            Ok(()) => {
                                responder.respond(SetPermissionModeResponse {})?;
                            }
                            Err(e) => {
                                responder.respond_with_error(acp_error_to_sdk(&e))?;
                            }
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        // --- catch-all for unhandled methods ---
        .on_receive_dispatch(
            async move |dispatch: Dispatch, cx: ConnectionTo<Client>| {
                dispatch.respond_with_error(Error::method_not_found(), cx)?;
                Ok(())
            },
            on_receive_dispatch!(),
        )
        .connect_to(transport)
        .await?;

    Ok(())
}

/// Map our `AcpError` to the SDK's `Error` type.
pub(crate) fn acp_error_to_sdk(e: &AcpError) -> Error {
    match e {
        AcpError::InvalidParams(msg) => {
            Error::invalid_params().data(serde_json::Value::String(msg.clone()))
        }
        AcpError::Internal(msg) => {
            Error::internal_error().data(serde_json::Value::String(msg.clone()))
        }
    }
}
