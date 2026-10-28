//! A native file-gated tool shared by history integration fixtures.
//!
//! Re-executing a large Rust test image makes its loader part of a simulated
//! tool. Use the platform shell instead; the real child publishes its own PID
//! and result. The caller's original budget covers startup and execution.
use std::io;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

pub struct NativeTool {
    child: Child,
    deadline: Instant,
}

impl NativeTool {
    pub fn spawn(
        started: &Path,
        release: &Path,
        result: &Path,
        content: &str,
        budget: Duration,
    ) -> io::Result<Self> {
        let deadline = Instant::now() + budget;
        let mut command = command();
        command
            .env("SCODE_NATIVE_TOOL_STARTED", started)
            .env("SCODE_NATIVE_TOOL_RELEASE", release)
            .env("SCODE_NATIVE_TOOL_RESULT", result)
            .env("SCODE_NATIVE_TOOL_CONTENT", content)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        Ok(Self {
            child: command.spawn()?,
            deadline,
        })
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= self.deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "native fixture tool did not finish within its original budget",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for NativeTool {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
fn command() -> Command {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        r#"set -eu
printf '%s' "$$" > "$SCODE_NATIVE_TOOL_STARTED"
while [ ! -f "$SCODE_NATIVE_TOOL_RELEASE" ]; do sleep 0.01; done
printf '%s' "$SCODE_NATIVE_TOOL_CONTENT" > "$SCODE_NATIVE_TOOL_RESULT""#,
    ]);
    command
}

#[cfg(windows)]
fn command() -> Command {
    let mut command = Command::new("powershell.exe");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        r#"$ErrorActionPreference = 'Stop'
$encoding = [System.Text.UTF8Encoding]::new($false)
[System.IO.File]::WriteAllText($env:SCODE_NATIVE_TOOL_STARTED, [string]$PID, $encoding)
while (-not [System.IO.File]::Exists($env:SCODE_NATIVE_TOOL_RELEASE)) { Start-Sleep -Milliseconds 10 }
[System.IO.File]::WriteAllText($env:SCODE_NATIVE_TOOL_RESULT, $env:SCODE_NATIVE_TOOL_CONTENT, $encoding)"#,
    ]);
    command
}
