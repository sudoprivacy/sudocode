//! Interrupt-during-bash parity: the tool comes back `interrupted`, the turn
//! ends without a follow-up model call, and the process still prints its JSON
//! and exits 0.
//!
//! Runs on Windows too — see `ArmedInterrupt` for how the interrupt is
//! delivered there, and `common/isolated_env.rs` for why `env_clear()` alone
//! isn't a survivable environment for a Windows child.

#[path = "common/isolated_env.rs"]
mod isolated_env;

use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use serde_json::Value;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn sigint_during_bash_tool_returns_interrupted_result_without_continuing_turn() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let base_url = server.base_url();

    let workspace = unique_temp_dir("interrupt-e2e");
    let config_home = workspace.join("config-home");
    let home = workspace.join("home");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    fs::create_dir_all(&config_home).expect("config home should exist");
    fs::create_dir_all(&home).expect("home should exist");
    write_config(&config_home, &base_url);

    // Arm the interrupt BEFORE `scode` exists. The window it has to land in is
    // the tool's `sleep 30`, which opens the moment the model returns its bash
    // call — so anything expensive done after the spawn is done inside that
    // window. On Windows arming compiles a P/Invoke shim, which on a loaded
    // runner outlasts 30s: the sleep finished, the turn completed, and the
    // interrupt had nothing left to interrupt. Paying the whole cost up front
    // is what makes that impossible rather than unlikely. See `ArmedInterrupt`.
    let interrupt = ArmedInterrupt::arm(&workspace);
    interrupt.wait_until_armed(Duration::from_secs(120));

    let prompt = format!("{SCENARIO_PREFIX}bash_interrupt_long_running");
    let mut command = Command::new(env!("CARGO_BIN_EXE_scode"));
    command
        .current_dir(&workspace)
        .env_clear()
        .env("SUDO_CODE_CONFIG_HOME", &config_home)
        .env("HOME", &home)
        .env("NO_COLOR", "1");
    for (key, value) in isolated_env::inherited_env() {
        command.env(key, value);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP — makes the child its own group leader, so
        // `ArmedInterrupt` can target it by pid without the console event also
        // hitting the test runner. See `ArmedInterrupt`.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    let child = command
        .args([
            "--auth",
            "api-key",
            "--model",
            "sonnet",
            "--permission-mode",
            "danger-full-access",
            "--allowedTools",
            "bash",
            "--output-format",
            "json",
            &prompt,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("scode should launch");

    let mut child = ChildGuard(child);

    // Generous: this is a liveness wait, not the assertion. Under a parallel
    // `cargo test --workspace` the machine is running many other `scode`
    // children, and 10s was not always enough for this one to start and reach
    // the mock server.
    wait_for_message_request(&runtime, &server, 1, Duration::from_secs(60));
    thread::sleep(Duration::from_millis(1500));
    assert_running(&mut child, "before the interrupt is fired");
    interrupt.fire(child.id());

    let status = wait_for_exit(&mut child, Duration::from_secs(60))
        .expect("scode should exit after the interrupt");
    let mut stdout = String::new();
    let mut stderr = String::new();
    child
        .stdout
        .take()
        .expect("stdout should be piped")
        .read_to_string(&mut stdout)
        .expect("stdout should read");
    child
        .stderr
        .take()
        .expect("stderr should be piped")
        .read_to_string(&mut stderr)
        .expect("stderr should read");

    assert!(
        status.success(),
        "scode should exit cleanly after interrupt\nstdout:\n{stdout}\n\nstderr:\n{stderr}"
    );

    let captured = runtime.block_on(server.captured_requests());
    let message_request_count = captured
        .iter()
        .filter(|request| request.path == "/v1/messages")
        .count();
    assert_eq!(
        message_request_count, 1,
        "interrupt should cancel the turn without a follow-up model call"
    );

    let parsed: Value = serde_json::from_str(&stdout).expect("stdout should be JSON");
    let tool_results = parsed["tool_results"]
        .as_array()
        .expect("tool_results should be an array");
    assert_eq!(tool_results.len(), 1);
    assert_eq!(tool_results[0]["tool_name"], "bash");
    assert_eq!(tool_results[0]["is_error"], true);

    let tool_output: Value = serde_json::from_str(
        tool_results[0]["output"]
            .as_str()
            .expect("tool output should be a JSON string"),
    )
    .expect("bash output should parse as JSON");
    assert_eq!(tool_output["interrupted"], true);
    assert_eq!(tool_output["returnCodeInterpretation"], "interrupted");
    assert_eq!(tool_output["stderr"], "Command interrupted by user");

    fs::remove_dir_all(&workspace).expect("workspace cleanup should succeed");
}

fn wait_for_message_request(
    runtime: &tokio::runtime::Runtime,
    server: &MockAnthropicService,
    expected: usize,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        let captured = runtime.block_on(server.captured_requests());
        let count = captured
            .iter()
            .filter(|request| request.path == "/v1/messages")
            .count();
        if count >= expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} /v1/messages request(s); saw {count}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(error) => panic!("failed to wait for scode: {error}"),
        }
    }
}

/// Keeps the spawned `scode` from outliving the test. Every assertion below
/// can panic, and a leaked `scode` holds a `sleep 30` grandchild that goes on
/// competing for a shared CI runner long after the test that spawned it.
struct ChildGuard(std::process::Child);

impl std::ops::Deref for ChildGuard {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Already reaped on the happy path; both calls are then no-ops.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Fail with what actually happened rather than with the interrupt's own
/// second-order symptom. An interrupt aimed at a process that has already
/// exited reports "could not attach to that pid", which reads like a bug in
/// the interrupt machinery when the real news is that `scode` finished the
/// turn early — a different failure with a different fix.
fn assert_running(child: &mut ChildGuard, when: &str) {
    let Ok(Some(status)) = child.try_wait() else {
        return;
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    panic!(
        "scode already exited {when} (status {status:?}); the interrupt had \
         nothing to interrupt, so the tool never reached its `sleep 30`\
         \nstdout:\n{stdout}\n\nstderr:\n{stderr}"
    );
}

/// An interrupt that is fully prepared but not yet delivered.
///
/// Split in two because *preparing* to interrupt can cost more than the
/// window the interrupt has to land in. The window here is the tool's
/// `sleep 30`; on Windows preparation means compiling a C# P/Invoke shim
/// (see the platform impl), which on a cold runner can take tens of seconds.
/// Doing that after the turn starts spends the window on the preparation and
/// delivers the signal to a process that has already finished — which is not
/// what the assertion is about. `arm` runs before the turn, `wait_until_armed`
/// confirms it finished, and only then does `fire` deliver.
struct ArmedInterrupt {
    #[cfg(windows)]
    helper: std::process::Child,
    #[cfg(windows)]
    ready_path: PathBuf,
    #[cfg(windows)]
    go_path: PathBuf,
}

/// Unix: `kill -INT`, the signal `SignalCancelGuard` installs a handler for.
/// Nothing to prepare, so arming is bookkeeping and the wait returns at once.
#[cfg(unix)]
impl ArmedInterrupt {
    fn arm(_workspace: &std::path::Path) -> Self {
        Self {}
    }

    fn wait_until_armed(&self, _timeout: Duration) {}

    fn fire(self, pid: u32) {
        let status = Command::new("kill")
            .arg("-INT")
            .arg(pid.to_string())
            .status()
            .expect("kill should launch");
        assert!(status.success(), "kill -INT should succeed");
    }
}

/// Windows has no `kill -INT`; the equivalent is a *console control event*,
/// with two constraints. It can only be raised by a process attached to the
/// target's console, and when addressed to one process group it must be
/// CTRL_BREAK — Windows disables Ctrl-C for a group created with
/// CREATE_NEW_PROCESS_GROUP, which is exactly how the child above is spawned so
/// the event reaches it alone and not the test runner. `scode` maps Ctrl-Break
/// to the same `EngineCommand::Cancel` as Ctrl-C (see `SignalCancelGuard`).
///
/// `GenerateConsoleCtrlEvent` is raw FFI and this workspace forbids `unsafe`,
/// so the call goes through PowerShell: stock tooling, no new dependency, and
/// it drives the real Win32 console-signal path rather than simulating it. The
/// helper must `FreeConsole` before `AttachConsole`, since a process can be
/// attached to only one console at a time.
#[cfg(windows)]
impl ArmedInterrupt {
    /// Start the helper and let it compile. `Add-Type -MemberDefinition`
    /// compiles C# at run time — the slow part, and the reason arming is
    /// separate from firing. The helper reports readiness and waits for the
    /// go-ahead through two marker files rather than its own stdio: it must
    /// `FreeConsole` before it can attach to the target's console, and
    /// nothing that depends on the helper's console can be relied on across
    /// that call.
    ///
    /// The target pid arrives in the go-ahead file rather than being baked
    /// into the script, which is what lets the whole compile happen before
    /// `scode` is even spawned. Baked in, arming could only start once the
    /// child had a pid — so the compile ran while the window it had to land
    /// in was already open, and on a loaded runner the `sleep 30` finished
    /// first and the turn completed with nothing to interrupt.
    fn arm(workspace: &std::path::Path) -> Self {
        const CTRL_BREAK_EVENT: u32 = 1;
        let ready_path = workspace.join("interrupt-helper-ready");
        let go_path = workspace.join("interrupt-helper-go");
        let script = format!(
            r#"
$signature = @'
[DllImport("kernel32.dll", SetLastError = true)] public static extern bool FreeConsole();
[DllImport("kernel32.dll", SetLastError = true)] public static extern bool AttachConsole(uint dwProcessId);
[DllImport("kernel32.dll", SetLastError = true)] public static extern bool GenerateConsoleCtrlEvent(uint dwCtrlEvent, uint dwProcessGroupId);
'@
$kernel32 = Add-Type -MemberDefinition $signature -Name 'Kernel32' -Namespace 'Interrupt' -PassThru
New-Item -ItemType File -Path '{ready}' -Force | Out-Null
$targetPid = 0
while ($targetPid -eq 0) {{
  if (Test-Path -LiteralPath '{go}') {{
    $raw = (Get-Content -LiteralPath '{go}' -Raw -ErrorAction SilentlyContinue)
    if ($raw) {{ $parsed = 0; if ([uint32]::TryParse($raw.Trim(), [ref]$parsed)) {{ $targetPid = $parsed }} }}
  }}
  if ($targetPid -eq 0) {{ Start-Sleep -Milliseconds 20 }}
}}
[void]$kernel32::FreeConsole()
if (-not $kernel32::AttachConsole($targetPid)) {{ exit 2 }}
if (-not $kernel32::GenerateConsoleCtrlEvent({CTRL_BREAK_EVENT}, $targetPid)) {{ exit 3 }}
exit 0
"#,
            ready = ready_path.display(),
            go = go_path.display(),
        );
        let helper = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("powershell should launch");
        Self {
            helper,
            ready_path,
            go_path,
        }
    }

    fn wait_until_armed(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.ready_path.exists() {
            assert!(
                Instant::now() < deadline,
                "console-interrupt helper did not finish compiling within {timeout:?}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Release the helper and wait for it to report what the Win32 calls did.
    fn fire(self, pid: u32) {
        let Self {
            helper, go_path, ..
        } = self;
        // The pid IS the go-ahead: the helper spins until this parses, so an
        // empty or half-written file simply keeps it waiting.
        fs::write(&go_path, pid.to_string().as_bytes()).expect("go marker should be written");
        let output = helper
            .wait_with_output()
            .expect("console-interrupt helper should exit");
        assert!(
            output.status.success(),
            "console Ctrl-Break to pid {pid} failed (exit {:?}; 2 = AttachConsole, 3 = GenerateConsoleCtrlEvent)\nstderr:\n{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

fn write_config(config_home: &std::path::Path, base_url: &str) {
    let sample = runtime::SAMPLE_SUDOCODE_JSON
        .replace("https://api.anthropic.com", base_url)
        .replace("<YOUR_ANTHROPIC_API_KEY>", "test-interrupt-key");
    fs::write(config_home.join("sudocode.json"), sample).expect("sudocode.json should be written");
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "scode-{label}-{}-{millis}-{counter}",
        std::process::id()
    ))
}
