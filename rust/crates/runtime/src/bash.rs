use std::cell::RefCell;
use std::io;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command as TokioCommand;
use tokio::runtime::Builder;

use crate::hooks::HookAbortSignal;
use crate::lane_events::{LaneEvent, ShipMergeMethod, ShipProvenance};
use crate::sandbox::{
    build_linux_sandbox_command, resolve_sandbox_status_for_request, FilesystemIsolationMode,
    SandboxConfig, SandboxStatus,
};
use crate::workspace_root::current_workspace_root;
use crate::ConfigLoader;

/// Default foreground subprocess timeout for tool-backed command execution.
///
/// Tool schemas still allow callers to provide a larger or smaller per-call
/// timeout; this default prevents unbounded foreground commands from pinning a
/// turn indefinitely when the model omits that field.
pub const DEFAULT_TOOL_SUBPROCESS_TIMEOUT_MS: u64 = 120_000;

/// Progress report emitted during streaming bash execution.
pub struct BashProgress<'a> {
    /// Latest output chunk (may contain multiple lines).
    pub output: &'a str,
    /// Cumulative line count so far.
    pub total_lines: usize,
    /// Cumulative byte count so far.
    pub total_bytes: usize,
}

/// Callback invoked periodically during bash command execution.
pub type BashProgressCallback = Box<dyn Fn(BashProgress<'_>) + Send>;

thread_local! {
    static BASH_PROGRESS_CALLBACK: RefCell<Option<BashProgressCallback>> = const { RefCell::new(None) };
}

/// Store a progress callback in thread-local storage.
///
/// The next call to [`execute_bash_with_abort`] on this thread will
/// consume (take) the callback and use it for streaming output.
pub fn set_bash_progress_callback(cb: BashProgressCallback) {
    BASH_PROGRESS_CALLBACK.with(|cell| {
        *cell.borrow_mut() = Some(cb);
    });
}

/// Remove any progress callback from thread-local storage without
/// invoking it. Safe to call even when no callback is set.
pub fn clear_bash_progress_callback() {
    BASH_PROGRESS_CALLBACK.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// Input schema for the built-in bash execution tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BashCommandInput {
    pub command: String,
    pub timeout: Option<u64>,
    pub description: Option<String>,
    #[serde(rename = "run_in_background")]
    pub run_in_background: Option<bool>,
    #[serde(rename = "dangerouslyDisableSandbox")]
    pub dangerously_disable_sandbox: Option<bool>,
    #[serde(rename = "namespaceRestrictions")]
    pub namespace_restrictions: Option<bool>,
    #[serde(rename = "isolateNetwork")]
    pub isolate_network: Option<bool>,
    #[serde(rename = "filesystemMode")]
    pub filesystem_mode: Option<FilesystemIsolationMode>,
    #[serde(rename = "allowedMounts")]
    pub allowed_mounts: Option<Vec<String>>,
}

/// Output returned from a bash tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BashCommandOutput {
    pub stdout: String,
    pub stderr: String,
    #[serde(rename = "rawOutputPath")]
    pub raw_output_path: Option<String>,
    pub interrupted: bool,
    #[serde(rename = "isImage")]
    pub is_image: Option<bool>,
    #[serde(rename = "backgroundTaskId")]
    pub background_task_id: Option<String>,
    #[serde(rename = "backgroundedByUser")]
    pub backgrounded_by_user: Option<bool>,
    #[serde(rename = "assistantAutoBackgrounded")]
    pub assistant_auto_backgrounded: Option<bool>,
    #[serde(rename = "dangerouslyDisableSandbox")]
    pub dangerously_disable_sandbox: Option<bool>,
    #[serde(rename = "returnCodeInterpretation")]
    pub return_code_interpretation: Option<String>,
    #[serde(rename = "noOutputExpected")]
    pub no_output_expected: Option<bool>,
    #[serde(rename = "structuredContent")]
    pub structured_content: Option<Vec<serde_json::Value>>,
    #[serde(rename = "persistedOutputPath")]
    pub persisted_output_path: Option<String>,
    #[serde(rename = "persistedOutputSize")]
    pub persisted_output_size: Option<u64>,
    #[serde(rename = "sandboxStatus")]
    pub sandbox_status: Option<SandboxStatus>,
}

/// Executes a shell command with the requested sandbox settings.
pub fn execute_bash(input: BashCommandInput) -> io::Result<BashCommandOutput> {
    execute_bash_with_abort(input, None)
}

/// Executes a shell command and cooperates with turn cancellation.
///
/// If a progress callback was previously stored via
/// [`set_bash_progress_callback`], it will be consumed and used for
/// streaming output during this invocation.
pub fn execute_bash_with_abort(
    input: BashCommandInput,
    abort_signal: Option<&HookAbortSignal>,
) -> io::Result<BashCommandOutput> {
    let on_progress = BASH_PROGRESS_CALLBACK.with(|cell| cell.borrow_mut().take());
    execute_bash_with_progress(input, abort_signal, on_progress)
}

/// Executes a shell command with an optional streaming progress callback.
///
/// When `on_progress` is `Some`, stdout is read line-by-line and the
/// callback is invoked roughly every second with the latest output chunk.
/// When `None`, the original non-streaming code path is used.
pub fn execute_bash_with_progress(
    input: BashCommandInput,
    abort_signal: Option<&HookAbortSignal>,
    on_progress: Option<BashProgressCallback>,
) -> io::Result<BashCommandOutput> {
    // The session's workspace root, not the process cwd: concurrent turns of
    // sessions in different directories each spawn their shell in their own
    // root (see `crate::workspace_root`).
    let cwd = current_workspace_root()?;
    let sandbox_status = sandbox_status_for_input(&input, &cwd);

    if input.run_in_background.unwrap_or(false) {
        let mut child = prepare_command(&input.command, &cwd, &sandbox_status, false);
        let child = child
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;

        return Ok(BashCommandOutput {
            stdout: String::new(),
            stderr: String::new(),
            raw_output_path: None,
            interrupted: false,
            is_image: None,
            background_task_id: Some(child.id().to_string()),
            backgrounded_by_user: Some(false),
            assistant_auto_backgrounded: Some(false),
            dangerously_disable_sandbox: input.dangerously_disable_sandbox,
            return_code_interpretation: None,
            no_output_expected: Some(true),
            structured_content: None,
            persisted_output_path: None,
            persisted_output_size: None,
            sandbox_status: Some(sandbox_status),
        });
    }

    // Pick async implementation: streaming when a progress callback is
    // provided, the original `command.output()` path otherwise.
    let run_async = |handle_or_runtime: AsyncRunner| -> io::Result<BashCommandOutput> {
        if on_progress.is_some() {
            handle_or_runtime.block_on(execute_bash_streaming(
                input,
                sandbox_status,
                cwd,
                abort_signal.cloned(),
                on_progress,
            ))
        } else {
            handle_or_runtime.block_on_dyn(Box::pin(execute_bash_async(
                input,
                sandbox_status,
                cwd,
                abort_signal.cloned(),
            )))
        }
    };

    // If we are already inside a tokio runtime (e.g. when run_turn is
    // driven by an outer block_on), use the current handle instead of
    // creating a nested runtime which would panic.
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        run_async(AsyncRunner::Handle(handle))
    } else {
        let runtime = Builder::new_current_thread().enable_all().build()?;
        run_async(AsyncRunner::Runtime(runtime))
    }
}

/// Helper to abstract over `tokio::runtime::Handle` vs owned `Runtime`.
enum AsyncRunner {
    Handle(tokio::runtime::Handle),
    Runtime(tokio::runtime::Runtime),
}

impl AsyncRunner {
    fn block_on<F: std::future::Future<Output = io::Result<BashCommandOutput>>>(
        self,
        future: F,
    ) -> io::Result<BashCommandOutput> {
        match self {
            Self::Handle(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
            Self::Runtime(runtime) => runtime.block_on(future),
        }
    }

    fn block_on_dyn(
        self,
        future: std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<BashCommandOutput>>>>,
    ) -> io::Result<BashCommandOutput> {
        match self {
            Self::Handle(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
            Self::Runtime(runtime) => runtime.block_on(future),
        }
    }
}

/// Detect git push to main and emit ship provenance event
fn detect_and_emit_ship_prepared(command: &str, cwd: &std::path::Path) {
    let trimmed = command.trim();
    // Simple detection: git push with main/master
    if trimmed.contains("git push") && (trimmed.contains("main") || trimmed.contains("master")) {
        // Emit ship.prepared event
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let provenance = ShipProvenance {
            source_branch: get_current_branch(cwd).unwrap_or_else(|| "unknown".to_string()),
            base_commit: get_head_commit(cwd).unwrap_or_default(),
            commit_count: 0, // Would need to calculate from range
            commit_range: "unknown..HEAD".to_string(),
            merge_method: ShipMergeMethod::DirectPush,
            actor: get_git_actor(cwd).unwrap_or_else(|| "unknown".to_string()),
            pr_number: None,
        };
        let _event = LaneEvent::ship_prepared(format!("{now}"), &provenance);
        // Log to stderr as interim routing before event stream integration
        eprintln!(
            "[ship.prepared] branch={} -> main, commits={}, actor={}",
            provenance.source_branch, provenance.commit_count, provenance.actor
        );
    }
}

fn get_current_branch(cwd: &std::path::Path) -> Option<String> {
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn get_head_commit(cwd: &std::path::Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

fn get_git_actor(cwd: &std::path::Path) -> Option<String> {
    let name = Command::new("git")
        .args(["config", "user.name"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())?;
    Some(name)
}

async fn execute_bash_async(
    input: BashCommandInput,
    sandbox_status: SandboxStatus,
    cwd: std::path::PathBuf,
    abort_signal: Option<HookAbortSignal>,
) -> io::Result<BashCommandOutput> {
    // Detect and emit ship provenance for git push operations
    detect_and_emit_ship_prepared(&input.command, &cwd);

    let mut command = prepare_tokio_command(&input.command, &cwd, &sandbox_status, true);
    command.stdin(Stdio::null());

    command.kill_on_drop(true);
    let timeout_ms = input.timeout.unwrap_or(DEFAULT_TOOL_SUBPROCESS_TIMEOUT_MS);
    let output = command.output();
    tokio::pin!(output);
    let timeout_sleep = tokio::time::sleep(Duration::from_millis(timeout_ms));
    tokio::pin!(timeout_sleep);
    let abort_wait = async {
        if let Some(signal) = abort_signal {
            signal.cancelled().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(abort_wait);

    let output = tokio::select! {
        biased;
        () = &mut abort_wait => {
            return Ok(interrupted_bash_output(
                "Command interrupted by user",
                "interrupted",
                input.dangerously_disable_sandbox,
                sandbox_status,
            ));
        }
        () = &mut timeout_sleep => {
            return Ok(interrupted_bash_output(
                &format!("Command exceeded timeout of {timeout_ms} ms"),
                "timeout",
                input.dangerously_disable_sandbox,
                sandbox_status,
            ));
        }
        result = &mut output => result?,
    };

    let stdout = truncate_output(&String::from_utf8_lossy(&output.stdout));
    let stderr = truncate_output(&String::from_utf8_lossy(&output.stderr));
    let no_output_expected = Some(stdout.trim().is_empty() && stderr.trim().is_empty());
    let return_code_interpretation = output.status.code().and_then(|code| {
        if code == 0 {
            None
        } else {
            Some(format!("exit_code:{code}"))
        }
    });

    Ok(BashCommandOutput {
        stdout,
        stderr,
        raw_output_path: None,
        interrupted: false,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation,
        no_output_expected,
        structured_content: None,
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: Some(sandbox_status),
    })
}

/// Streaming variant of [`execute_bash_async`].
///
/// Pipes stdout and stderr from the child process and reads them
/// line-by-line. When `on_progress` is `Some`, the callback is invoked
/// roughly every second with the latest stdout chunk. All output is
/// collected for the final [`BashCommandOutput`].
async fn execute_bash_streaming(
    input: BashCommandInput,
    sandbox_status: SandboxStatus,
    cwd: std::path::PathBuf,
    abort_signal: Option<HookAbortSignal>,
    on_progress: Option<BashProgressCallback>,
) -> io::Result<BashCommandOutput> {
    detect_and_emit_ship_prepared(&input.command, &cwd);

    let mut command = prepare_tokio_command(&input.command, &cwd, &sandbox_status, true);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);

    let timeout_ms = input.timeout.unwrap_or(DEFAULT_TOOL_SUBPROCESS_TIMEOUT_MS);
    let mut child = command.spawn()?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let mut stdout_reader = tokio::io::BufReader::new(stdout);
    let mut stderr_reader = tokio::io::BufReader::new(stderr);

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let mut total_lines: usize = 0;
    let mut total_bytes: usize = 0;
    let mut progress_chunk = String::new();
    let mut last_progress = tokio::time::Instant::now();

    const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

    let timeout_deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let abort_wait = async {
        if let Some(signal) = &abort_signal {
            signal.cancelled().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(abort_wait);

    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut stdout_line = String::new();
    let mut stderr_line = String::new();

    loop {
        if stdout_done && stderr_done {
            break;
        }

        tokio::select! {
            biased;
            () = &mut abort_wait => {
                let _ = child.kill().await;
                return Ok(interrupted_bash_output(
                    "Command interrupted by user",
                    "interrupted",
                    input.dangerously_disable_sandbox,
                    sandbox_status,
                ));
            }
            _ = tokio::time::sleep_until(timeout_deadline) => {
                let _ = child.kill().await;
                return Ok(interrupted_bash_output(
                    &format!("Command exceeded timeout of {timeout_ms} ms"),
                    "timeout",
                    input.dangerously_disable_sandbox,
                    sandbox_status,
                ));
            }
            result = stdout_reader.read_line(&mut stdout_line), if !stdout_done => {
                match result {
                    Ok(0) => stdout_done = true,
                    Ok(n) => {
                        total_bytes += n;
                        total_lines += 1;
                        stdout_buf.push_str(&stdout_line);
                        progress_chunk.push_str(&stdout_line);
                        stdout_line.clear();

                        if let Some(ref cb) = on_progress {
                            if last_progress.elapsed() >= PROGRESS_INTERVAL {
                                cb(BashProgress {
                                    output: &progress_chunk,
                                    total_lines,
                                    total_bytes,
                                });
                                progress_chunk.clear();
                                last_progress = tokio::time::Instant::now();
                            }
                        }
                    }
                    Err(_) => stdout_done = true,
                }
            }
            result = stderr_reader.read_line(&mut stderr_line), if !stderr_done => {
                match result {
                    Ok(0) => stderr_done = true,
                    Ok(n) => {
                        total_bytes += n;
                        stderr_buf.push_str(&stderr_line);
                        stderr_line.clear();
                    }
                    Err(_) => stderr_done = true,
                }
            }
        }
    }

    // Flush any remaining progress chunk
    if let Some(ref cb) = on_progress {
        if !progress_chunk.is_empty() {
            cb(BashProgress {
                output: &progress_chunk,
                total_lines,
                total_bytes,
            });
        }
    }

    // Wait for the child to fully exit
    let status = child.wait().await?;

    let stdout = truncate_output(&stdout_buf);
    let stderr = truncate_output(&stderr_buf);
    let no_output_expected = Some(stdout.trim().is_empty() && stderr.trim().is_empty());
    let return_code_interpretation = status.code().and_then(|code| {
        if code == 0 {
            None
        } else {
            Some(format!("exit_code:{code}"))
        }
    });

    Ok(BashCommandOutput {
        stdout,
        stderr,
        raw_output_path: None,
        interrupted: false,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox: input.dangerously_disable_sandbox,
        return_code_interpretation,
        no_output_expected,
        structured_content: None,
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: Some(sandbox_status),
    })
}

fn interrupted_bash_output(
    stderr: &str,
    return_code_interpretation: &str,
    dangerously_disable_sandbox: Option<bool>,
    sandbox_status: SandboxStatus,
) -> BashCommandOutput {
    BashCommandOutput {
        stdout: String::new(),
        stderr: stderr.to_string(),
        raw_output_path: None,
        interrupted: true,
        is_image: None,
        background_task_id: None,
        backgrounded_by_user: None,
        assistant_auto_backgrounded: None,
        dangerously_disable_sandbox,
        return_code_interpretation: Some(return_code_interpretation.to_string()),
        no_output_expected: Some(true),
        structured_content: None,
        persisted_output_path: None,
        persisted_output_size: None,
        sandbox_status: Some(sandbox_status),
    }
}

fn sandbox_status_for_input(input: &BashCommandInput, cwd: &std::path::Path) -> SandboxStatus {
    let config = ConfigLoader::default_for(cwd).load().map_or_else(
        |_| SandboxConfig::default(),
        |runtime_config| runtime_config.sandbox().clone(),
    );
    let request = config.resolve_request(
        input.dangerously_disable_sandbox.map(|disabled| !disabled),
        input.namespace_restrictions,
        input.isolate_network,
        input.filesystem_mode,
        input.allowed_mounts.clone(),
    );
    resolve_sandbox_status_for_request(&request, cwd)
}

fn prepare_command(
    command: &str,
    cwd: &std::path::Path,
    sandbox_status: &SandboxStatus,
    create_dirs: bool,
) -> Command {
    if create_dirs {
        prepare_sandbox_dirs(cwd);
    }

    if let Some(launcher) = build_linux_sandbox_command(command, cwd, sandbox_status) {
        let mut prepared = Command::new(launcher.program);
        prepared.args(launcher.args);
        prepared.current_dir(cwd);
        prepared.envs(launcher.env);
        return prepared;
    }

    let mut prepared = Command::new("sh");
    prepared.arg("-lc").arg(command).current_dir(cwd);
    if sandbox_status.filesystem_active {
        prepared.env("HOME", cwd.join(".sandbox-home"));
        prepared.env("TMPDIR", cwd.join(".sandbox-tmp"));
    }
    prepared
}

fn prepare_tokio_command(
    command: &str,
    cwd: &std::path::Path,
    sandbox_status: &SandboxStatus,
    create_dirs: bool,
) -> TokioCommand {
    if create_dirs {
        prepare_sandbox_dirs(cwd);
    }

    let mut prepared =
        if let Some(launcher) = build_linux_sandbox_command(command, cwd, sandbox_status) {
            let mut cmd = TokioCommand::new(launcher.program);
            cmd.args(launcher.args);
            cmd.envs(launcher.env);
            cmd
        } else {
            let mut cmd = TokioCommand::new("sh");
            cmd.arg("-lc").arg(command);
            if sandbox_status.filesystem_active {
                cmd.env("HOME", cwd.join(".sandbox-home"));
                cmd.env("TMPDIR", cwd.join(".sandbox-tmp"));
            }
            cmd
        };

    prepared.current_dir(cwd);
    prepared.stdin(Stdio::null());
    prepared
}

fn prepare_sandbox_dirs(cwd: &std::path::Path) {
    let _ = std::fs::create_dir_all(cwd.join(".sandbox-home"));
    let _ = std::fs::create_dir_all(cwd.join(".sandbox-tmp"));
}

// ---------------------------------------------------------------------------
// Bash with file change tracking
// ---------------------------------------------------------------------------

use crate::file_snapshot::FileChangeSnapshotWithMtime;

/// Result of bash execution with file change tracking.
#[derive(Debug)]
pub struct BashWithTrackingResult {
    /// The original bash output.
    pub output: BashCommandOutput,

    /// File changes detected during execution.
    pub file_changes: FileChangeSnapshotWithMtime,
}

/// Execute a bash command with file change tracking.
///
/// Captures a snapshot before and after execution to detect
/// files created or modified by the command.
pub fn execute_bash_with_tracking(
    input: BashCommandInput,
    workspace_root: Option<&std::path::Path>,
) -> io::Result<BashWithTrackingResult> {
    let cwd = workspace_root
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| current_workspace_root().unwrap_or_default());

    // Capture before snapshot
    let mut snapshot = FileChangeSnapshotWithMtime::capture_before(&cwd);

    // Execute the command
    let output = execute_bash(input)?;

    // Capture after snapshot
    snapshot.capture_after(&cwd);

    Ok(BashWithTrackingResult {
        output,
        file_changes: snapshot,
    })
}

// `#[cfg(unix)]` because every test in this module exercises
// `execute_bash`, which spawns `sh -c "..."` (see line 338/364
// `Command::new("sh")`). On Windows `sh.exe` is not in PATH unless
// the developer has installed Git Bash; CI's `windows-latest`
// image does not. The production bash tool itself is therefore
// Unix-only by design — making it cross-platform would mean
// teaching `execute_bash` to detect platform and translate
// commands (or refuse on Windows with a clear diagnostic), which
// is a separate runtime-side product decision outside the scope of
// this PR. Tests covered: simple `printf 'hello'`, sandbox-disable
// path, stdin-redirect-to-null, abort-signal interrupt — all
// require sh.
#[cfg(all(test, unix))]
mod tests {
    use super::{
        execute_bash, execute_bash_with_abort, execute_bash_with_progress, BashCommandInput,
        BashProgressCallback,
    };
    use crate::hooks::HookAbortSignal;
    use crate::sandbox::FilesystemIsolationMode;

    #[test]
    fn executes_simple_command() {
        let output = execute_bash(BashCommandInput {
            command: String::from("printf 'hello'"),
            timeout: Some(1_000),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(false),
            namespace_restrictions: Some(false),
            isolate_network: Some(false),
            filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
            allowed_mounts: None,
        })
        .expect("bash command should execute");

        assert_eq!(output.stdout, "hello");
        assert!(!output.interrupted);
        assert!(output.sandbox_status.is_some());
    }

    #[test]
    fn disables_sandbox_when_requested() {
        let output = execute_bash(BashCommandInput {
            command: String::from("printf 'hello'"),
            timeout: Some(1_000),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(true),
            namespace_restrictions: None,
            isolate_network: None,
            filesystem_mode: None,
            allowed_mounts: None,
        })
        .expect("bash command should execute");

        assert!(!output.sandbox_status.expect("sandbox status").enabled);
    }

    #[test]
    fn abort_signal_interrupts_foreground_command() {
        let abort_signal = HookAbortSignal::new();
        abort_signal.abort();

        let output = execute_bash_with_abort(
            BashCommandInput {
                command: String::from("sleep 5"),
                timeout: Some(10_000),
                description: None,
                run_in_background: Some(false),
                dangerously_disable_sandbox: Some(false),
                namespace_restrictions: Some(false),
                isolate_network: Some(false),
                filesystem_mode: Some(FilesystemIsolationMode::WorkspaceOnly),
                allowed_mounts: None,
            },
            Some(&abort_signal),
        )
        .expect("bash command should return interrupted output");

        assert!(output.interrupted);
        assert_eq!(
            output.return_code_interpretation.as_deref(),
            Some("interrupted")
        );
    }

    #[test]
    fn prevents_stdin_hangs_by_redirecting_to_null() {
        let output = execute_bash(BashCommandInput {
            command: String::from("cat"),
            timeout: Some(2_000),
            description: None,
            run_in_background: Some(false),
            dangerously_disable_sandbox: Some(true),
            namespace_restrictions: None,
            isolate_network: None,
            filesystem_mode: None,
            allowed_mounts: None,
        })
        .expect("bash command should execute cleanly");

        assert!(
            !output.interrupted,
            "Command hung and was cut off by the timeout!"
        );
    }

    #[test]
    fn streaming_progress_callback_invoked() {
        use std::sync::{Arc, Mutex};

        let calls: Arc<Mutex<Vec<(String, usize, usize)>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_clone = calls.clone();
        let on_progress: Option<BashProgressCallback> = Some(Box::new(move |progress| {
            calls_clone.lock().unwrap().push((
                progress.output.to_string(),
                progress.total_lines,
                progress.total_bytes,
            ));
        }));

        // Use a command that produces multiple lines with a small delay
        // so the 1-second progress interval fires at least once, plus
        // the final flush.
        let output = execute_bash_with_progress(
            BashCommandInput {
                command: String::from("for i in 1 2 3; do echo \"line $i\"; sleep 0.5; done"),
                timeout: Some(10_000),
                description: None,
                run_in_background: Some(false),
                dangerously_disable_sandbox: Some(true),
                namespace_restrictions: None,
                isolate_network: None,
                filesystem_mode: None,
                allowed_mounts: None,
            },
            None,
            on_progress,
        )
        .expect("streaming bash command should execute");

        assert!(!output.interrupted);
        assert!(output.stdout.contains("line 1"));
        assert!(output.stdout.contains("line 2"));
        assert!(output.stdout.contains("line 3"));

        let recorded = calls.lock().unwrap();
        // The callback must have been invoked at least once (the final
        // flush fires even if the interval never elapsed).
        assert!(
            !recorded.is_empty(),
            "progress callback should have been invoked at least once"
        );
        // The last call should report all 3 lines
        let last = recorded.last().unwrap();
        assert_eq!(last.1, 3, "total_lines should be 3 after all output");
    }

    #[test]
    fn streaming_without_callback_still_works() {
        let output = execute_bash_with_progress(
            BashCommandInput {
                command: String::from("printf 'streaming-none'"),
                timeout: Some(2_000),
                description: None,
                run_in_background: Some(false),
                dangerously_disable_sandbox: Some(true),
                namespace_restrictions: None,
                isolate_network: None,
                filesystem_mode: None,
                allowed_mounts: None,
            },
            None,
            None,
        )
        .expect("bash with no callback should still work");

        assert_eq!(output.stdout, "streaming-none");
        assert!(!output.interrupted);
    }
}

/// Maximum output bytes before truncation (16 KiB, matching upstream).
const MAX_OUTPUT_BYTES: usize = 16_384;

/// Truncate output to `MAX_OUTPUT_BYTES`, appending a marker when trimmed.
fn truncate_output(s: &str) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s.to_string();
    }
    // Find the last valid UTF-8 boundary at or before MAX_OUTPUT_BYTES
    let mut end = MAX_OUTPUT_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = s[..end].to_string();
    truncated.push_str("\n\n[output truncated — exceeded 16384 bytes]");
    truncated
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn short_output_unchanged() {
        let s = "hello world";
        assert_eq!(truncate_output(s), s);
    }

    #[test]
    fn long_output_truncated() {
        let s = "x".repeat(20_000);
        let result = truncate_output(&s);
        assert!(result.len() < 20_000);
        assert!(result.ends_with("[output truncated — exceeded 16384 bytes]"));
    }

    #[test]
    fn exact_boundary_unchanged() {
        let s = "a".repeat(MAX_OUTPUT_BYTES);
        assert_eq!(truncate_output(&s), s);
    }

    #[test]
    fn one_over_boundary_truncated() {
        let s = "a".repeat(MAX_OUTPUT_BYTES + 1);
        let result = truncate_output(&s);
        assert!(result.contains("[output truncated"));
    }
}
