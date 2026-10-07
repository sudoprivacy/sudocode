//! One finite task over the existing engine seam. No terminal/UI input.
use std::io::{self, IsTerminal, Read, Write};
use std::sync::{
    atomic::{AtomicI32, Ordering},
    mpsc, Arc,
};
use std::time::{Duration, Instant};

use engine_events::{EngineCommand, EngineEvent, PermissionPromptDecision};
use serde_json::{json, Value};

use super::args::AllowedToolSet;
use crate::LiveCli;

type Error = Box<dyn std::error::Error>;
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(3);
const INPUT_TIMEOUT: Duration = Duration::from_secs(30);
const INPUT_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputFormat {
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeadlessOptions {
    pub prompt: String,
    pub model: String,
    pub allowed_tools: Option<AllowedToolSet>,
    pub permission_mode: runtime::PermissionMode,
    pub reasoning_effort: Option<String>,
    pub auth_mode: Option<engine_core::AuthMode>,
    pub allow_broad_cwd: bool,
    pub base_commit: Option<String>,
    pub resume: Option<String>,
    pub format: OutputFormat,
    pub verbose: bool,
}

#[derive(Debug)]
pub(crate) struct ReportedExit(pub i32);
impl std::fmt::Display for ReportedExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "headless exit {}", self.0)
    }
}
impl std::error::Error for ReportedExit {}

pub(crate) fn requested(args: &[String]) -> bool {
    args.iter()
        .take_while(|a| a.as_str() != "--")
        .any(|a| a == "-p" || a == "--print")
}

pub(crate) fn report_argument_error(args: &[String], message: &str) {
    let structured = args
        .iter()
        .any(|a| a == "--output-format=json" || a == "--output-format=stream-json")
        || args
            .windows(2)
            .any(|w| w[0] == "--output-format" && matches!(w[1].as_str(), "json" | "stream-json"));
    if structured {
        let _ = write_json(
            &mut io::stdout().lock(),
            &json!({
                "type":"result", "schema_version":1, "subtype":"invalid_input", "is_error":true,
                "result":"", "error":message, "session_id":null, "num_turns":0, "permission_denials":[]
            }),
        );
    } else {
        eprintln!("{message}");
    }
}

fn write_json(out: &mut impl Write, value: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    out.write_all(&bytes)?;
    out.flush()
}

/// A blocking reader thread is deliberately confined to input: the main thread
/// enforces both deadlines even when the producer never closes its pipe.
fn read_input(prompt: &str, signal: &AtomicI32) -> Result<String, Error> {
    if io::stdin().is_terminal() {
        if prompt.trim().is_empty() {
            return Err("empty input: -p requires a task or finite piped stdin".into());
        }
        return Ok(prompt.into());
    }
    let (tx, rx) = mpsc::sync_channel(2);
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        loop {
            let mut chunk = vec![0; 8192];
            let read = stdin.read(&mut chunk).map(|n| {
                chunk.truncate(n);
                chunk
            });
            let done = read.as_ref().map_or(true, Vec::is_empty);
            if tx.send(read).is_err() || done {
                break;
            }
        }
    });
    let start = Instant::now();
    let mut bytes = Vec::new();
    loop {
        if signal.load(Ordering::SeqCst) != 0 {
            return Err("cancelled while reading stdin".into());
        }
        let timeout = if bytes.is_empty() {
            FIRST_BYTE_TIMEOUT
        } else {
            INPUT_TIMEOUT
        };
        if start.elapsed() >= timeout {
            return Err(if bytes.is_empty() { "no stdin data received within 3s; close the input pipe or redirect stdin from /dev/null" } else { "stdin did not reach EOF within 30s" }.into());
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(chunk)) if chunk.is_empty() => break,
            Ok(Ok(chunk)) => {
                if bytes.len() + chunk.len() > INPUT_LIMIT {
                    return Err("stdin exceeds the 16 MiB limit".into());
                }
                bytes.extend(chunk);
            }
            Ok(Err(error)) => return Err(error.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("stdin reader disconnected".into())
            }
        }
    }
    let input = String::from_utf8(bytes).map_err(|_| "stdin is not valid UTF-8")?;
    if prompt.trim().is_empty() && input.trim().is_empty() {
        return Err("empty input: -p requires a task or finite piped stdin".into());
    }
    Ok(if input.is_empty() {
        prompt.into()
    } else if prompt.trim().is_empty() {
        input
    } else {
        format!("{prompt}\n\n<stdin>\n{input}\n</stdin>")
    })
}

/// Registered before reading stdin or building the engine. The pump checks the
/// atomic as well as receiving events, so an idle provider cannot swallow SIGTERM.
struct Signals {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Signals {
    fn install(target: Arc<AtomicI32>) -> Result<Self, Error> {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (ready, wait) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready.send(Err(e.to_string()));
                    return;
                }
            };
            rt.block_on(async move {
                #[cfg(unix)]
                let mut terminate = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(signal) => signal,
                    Err(e) => { let _ = ready.send(Err(e.to_string())); return; }
                };
                #[cfg(unix)]
                let mut interrupt = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
                    Ok(signal) => signal,
                    Err(e) => { let _ = ready.send(Err(e.to_string())); return; }
                };
                let _ = ready.send(Ok(()));
                #[cfg(unix)]
                tokio::select! {
                    _ = terminate.recv() => { target.store(143, Ordering::SeqCst); }
                    _ = interrupt.recv() => { target.store(130, Ordering::SeqCst); }
                    _ = stopped => {}
                }
                #[cfg(not(unix))]
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => { target.store(130, Ordering::SeqCst); }
                    () = crate::ctrl_break_cancel_signal() => { target.store(130, Ordering::SeqCst); }
                    _ = stopped => {}
                }
            });
        });
        wait.recv()?.map_err(|e| -> Error { e.into() })?;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
struct Messages {
    content: Vec<Value>,
    text: String,
    final_text: String,
    count: usize,
}
impl Messages {
    fn flush_text(&mut self) {
        if !self.text.is_empty() {
            self.content
                .push(json!({"type":"text", "text":std::mem::take(&mut self.text)}));
        }
    }
    fn complete(&mut self, session: &str) -> Value {
        self.flush_text();
        self.count += 1;
        // Only the last complete, tool-free assistant message is a final answer.
        self.final_text = if self.content.iter().any(|b| b["type"] == "tool_use") {
            String::new()
        } else {
            self.content
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("")
        };
        json!({"type":"assistant", "session_id":session, "message": {
            "id":format!("{session}:assistant:{}", self.count), "role":"assistant",
            "content":std::mem::take(&mut self.content)
        }})
    }
}

pub(crate) fn run(options: HeadlessOptions) -> Result<(), Error> {
    let started = Instant::now();

    let mut result = json!({"type":"result", "schema_version":1, "subtype":"success", "is_error":false,
        "result":"", "session_id":null, "num_turns":0, "permission_denials":[]});
    let mut exit_code = 0;
    let mut stage = "startup_error";
    let signal_code = Arc::new(AtomicI32::new(0));
    let mut signals = None;
    let mut out = super::headless_output::Output::new(signal_code.clone());
    let execution = (|| -> Result<(), Error> {
        signals = Some(Signals::install(signal_code.clone())?);
        stage = "invalid_input";
        let prompt = read_input(&options.prompt, &signal_code)?;
        // Broad-directory approval is explicit, even when stdin is a terminal.
        if !options.allow_broad_cwd {
            if let Some(cwd) = super::git::detect_broad_cwd() {
                return Err(format!(
                    "broad working directory {}; use --allow-broad-cwd",
                    cwd.display()
                )
                .into());
            }
        }
        stage = "startup_error";
        crate::run_stale_base_preflight(options.base_commit.as_deref());
        tools::declare_finite_task_mode();
        let cli = LiveCli::new(
            options.model,
            true,
            options.allowed_tools,
            options.permission_mode,
            options.reasoning_effort,
            options.auth_mode,
        )?;
        let execution = (|| -> Result<(), Error> {
            if let Some(reference) = options.resume {
                cli.lifecycle.resume_session(&reference)?;
            }
            let session = cli.lifecycle.session_handle().id;
            result["session_id"] = json!(session);
            if options.format == OutputFormat::StreamJson {
                write_json(
                    &mut out,
                    &json!({"type":"system", "subtype":"init", "schema_version":1,
                    "session_id":session, "model":cli.lifecycle.current_model(), "partial_messages":false}),
                )?;
            }
            let blocks = runtime::image_input::prompt_blocks(&prompt, &runtime::StdFsBackend)?;
            cli.engine_handle
                .commands
                .send(EngineCommand::Prompt { blocks })?;
            result["num_turns"] = json!(1);
            stage = "runtime_error";
            let mut messages = Messages::default();
            let run_id = format!(
                "{session}:{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_nanos()
            );
            let mut usage_observed = false;
            let mut cancel_sent = false;
            let mut needs_input = false;
            let mut cancelled_at = None;
            loop {
                if cancelled_at.is_some_and(|at: Instant| at.elapsed() > Duration::from_secs(10)) {
                    stage = "cancelled";
                    return Err("cancellation did not complete within 10s".into());
                }
                if signal_code.load(Ordering::SeqCst) != 0 && !cancel_sent {
                    cli.engine_handle.commands.send(EngineCommand::Cancel)?;
                    cancel_sent = true;
                    cancelled_at = Some(Instant::now());
                }
                let event = match cli
                    .engine_handle
                    .events
                    .recv_timeout(Duration::from_millis(50))
                {
                    Ok(event) => event,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err("engine disconnected without TurnComplete".into())
                    }
                };
                match event {
                    EngineEvent::TextDelta { text } => messages.text.push_str(&text),
                    EngineEvent::ToolCall { id, name, input } => {
                        messages.flush_text();
                        let input: Value = serde_json::from_str(&input)?;
                        if !input.is_object() {
                            return Err("tool input must be a JSON object".into());
                        }
                        messages
                            .content
                            .push(json!({"type":"tool_use", "id":id, "name":name, "input":input}));
                    }
                    EngineEvent::MessageComplete => {
                        let mut message = messages.complete(&run_id);
                        message["session_id"] = json!(session);
                        if options.format == OutputFormat::StreamJson {
                            write_json(&mut out, &message)?;
                        }
                    }
                    EngineEvent::ToolResult {
                        id,
                        name,
                        output,
                        is_error,
                    } => {
                        if options.format == OutputFormat::StreamJson {
                            write_json(
                                &mut out,
                                &json!({"type":"user", "session_id":session,
                                "message":{"id":format!("{run_id}:tool:{id}"), "role":"user", "content":[{"type":"tool_result", "tool_use_id":id,
                                "tool_name":name, "content":output, "is_error":is_error}]}}),
                            )?;
                        }
                    }
                    EngineEvent::PermissionRequest { id, .. } => {
                        cli.engine_handle
                            .commands
                            .send(EngineCommand::PermissionAnswer {
                                id,
                                decision: PermissionPromptDecision::Deny {
                                    reason: "interactive approval unavailable in print mode".into(),
                                },
                            })?;
                    }
                    EngineEvent::PermissionDenied {
                        id,
                        name,
                        input,
                        reason,
                    } => {
                        result["permission_denials"].as_array_mut().unwrap().push(json!({
                            "tool_use_id":id, "tool_name":name,
                            "input":serde_json::from_str::<Value>(&input).unwrap_or(Value::Null), "reason":reason,
                        }));
                    }
                    EngineEvent::QuestionRequest { id, .. } => {
                        needs_input = true;
                        cancelled_at = Some(Instant::now());
                        cli.engine_handle.commands.send(EngineCommand::Cancel)?;
                        cli.engine_handle
                            .commands
                            .send(EngineCommand::QuestionAnswer {
                                id,
                                answers: Err(
                                    "headless mode cannot answer an interactive question".into()
                                ),
                            })?;
                    }
                    EngineEvent::Usage(_) => usage_observed = true,
                    EngineEvent::TurnComplete(tc) => {
                        result["result"] = json!(messages.final_text);
                        result["model_round_trips"] = json!(tc.iterations);
                        if usage_observed {
                            result["usage"] = json!({"input_tokens":tc.turn_usage.input_tokens, "output_tokens":tc.turn_usage.output_tokens,
                            "cache_read_input_tokens":tc.turn_usage.cache_read_input_tokens,
                            "cache_creation_input_tokens":tc.turn_usage.cache_creation_input_tokens});
                        }
                        if needs_input {
                            stage = "needs_input";
                            return Err("task requires interactive input".into());
                        }
                        if tc.cancelled {
                            stage = "cancelled";
                            return Err("task cancelled".into());
                        }
                        if messages.count == 0
                            || !messages.text.is_empty()
                            || !messages.content.is_empty()
                        {
                            return Err("engine ended without a complete assistant message".into());
                        }
                        if messages.final_text.is_empty()
                            && !result["permission_denials"].as_array().unwrap().is_empty()
                        {
                            stage = "blocked";
                            return Err("task blocked by required permission".into());
                        }
                        return Ok(());
                    }
                    EngineEvent::Error { message } => return Err(message.into()),
                    EngineEvent::Notice { text } if options.verbose => eprintln!("{text}"),
                    _ => {}
                }
            }
        })();
        // Always cancel/close even after broken stdout or malformed events. Drop
        // the lifecycle owner before awaiting channel closure so MCP teardown runs.
        let _ = cli.engine_handle.commands.send(EngineCommand::Cancel);
        let _ = cli.engine_handle.commands.send(EngineCommand::Close);
        let crate::LiveCli {
            engine_handle,
            lifecycle,
            ..
        } = cli;
        drop(lifecycle);
        if !engine_handle.shutdown(Duration::from_secs(10)) && execution.is_ok() {
            stage = "cleanup_timeout";
            return Err("engine teardown exceeded 10s".into());
        }
        execution
    })();
    if let Err(error) = execution {
        exit_code = if stage == "invalid_input" { 2 } else { 1 };
        result["is_error"] = json!(true);
        result["subtype"] = json!(stage);
        result["kind"] = json!(crate::classify_error_kind(&error.to_string()));
        result["error"] = json!(error.to_string());
    }
    let signal = signal_code.load(Ordering::SeqCst);
    if signal != 0 {
        exit_code = signal;
        result["is_error"] = json!(true);
        result["subtype"] = json!("cancelled");
    }
    result["duration_ms"] = json!(started.elapsed().as_millis());
    let written = if options.format == OutputFormat::Text {
        if exit_code == 0 {
            writeln!(out, "{}", result["result"].as_str().unwrap_or_default())
                .and_then(|()| out.flush())
        } else {
            eprintln!(
                "[error-kind: {}]\n{}: {}",
                result["kind"].as_str().unwrap_or("unknown"),
                result["subtype"].as_str().unwrap_or("error"),
                result["error"].as_str().unwrap_or("cancelled")
            );
            Ok(())
        }
    } else {
        write_json(&mut out, &result)
    };
    if let Err(error) = written {
        eprintln!("headless output failed: {error}");
        if !matches!(exit_code, 130 | 143) {
            exit_code = 1;
        }
    }
    drop(signals);
    if exit_code == 0 {
        Ok(())
    } else {
        Err(Box::new(ReportedExit(exit_code)))
    }
}
