//! Freeze and resume a whole process, for tests that need a lease holder to
//! stop renewing without dying.
//!
//! A killed holder releases its lock; a frozen one keeps it while doing no
//! work, which is the harder case the reader-lease handoff has to survive: a
//! laptop asleep, a paused VM, a debugger break, a `SIGSTOP`. The two platforms
//! reach that state differently -- Unix sends `SIGSTOP`/`SIGCONT`, Windows asks
//! ntdll to suspend every thread -- but both stop at the same observable point:
//! the process still exists and still holds what it held, and no thread runs.
//!
//! `unsafe_code = "forbid"` holds in this crate, so neither path makes a raw FFI
//! suspend call from Rust. Unix goes through `nix`'s safe `kill`; Windows shells
//! out to PowerShell, which owns the single P/Invoke.

#![allow(dead_code)] // each test file uses a subset

use std::io;

/// A frozen process. Dropping it resumes the process, so a test that panics
/// while something is frozen never leaves a stopped process behind -- on CI
/// least of all. `resume` is where a failure to resume is reported; `drop`
/// only makes the attempt.
#[must_use = "the process stays frozen until this guard is resumed or dropped"]
pub struct FreezeGuard {
    pid: u32,
    resumed: bool,
}

impl FreezeGuard {
    /// Resume now and surface any failure, rather than leaving it to `drop`.
    ///
    /// # Errors
    /// When the platform resume call fails.
    pub fn resume(mut self) -> io::Result<()> {
        self.resume_in_place()
    }

    fn resume_in_place(&mut self) -> io::Result<()> {
        if self.resumed {
            return Ok(());
        }
        resume_pid(self.pid)?;
        self.resumed = true;
        Ok(())
    }
}

impl Drop for FreezeGuard {
    fn drop(&mut self) {
        let _ = self.resume_in_place();
    }
}

/// Freeze `pid`: every thread stops, the process stays alive and keeps the
/// locks and handles it held. The process is confirmed stopped before this
/// returns, so a caller that gets `Ok` knows it is actually frozen, not merely
/// signalled.
///
/// # Errors
/// When the process cannot be frozen, or the freeze cannot be confirmed.
pub fn freeze(pid: u32) -> io::Result<FreezeGuard> {
    freeze_pid(pid)?;
    Ok(FreezeGuard {
        pid,
        resumed: false,
    })
}

#[cfg(unix)]
fn freeze_pid(pid: u32) -> io::Result<()> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    kill(
        Pid::from_raw(i32::try_from(pid).expect("OS PID fits i32")),
        Signal::SIGSTOP,
    )
    .map_err(|e| io::Error::other(format!("SIGSTOP {pid}: {e}")))?;
    wait_until_stopped(pid)
}

#[cfg(unix)]
fn resume_pid(pid: u32) -> io::Result<()> {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;
    kill(
        Pid::from_raw(i32::try_from(pid).expect("OS PID fits i32")),
        Signal::SIGCONT,
    )
    .map_err(|e| io::Error::other(format!("SIGCONT {pid}: {e}")))
}

/// Confirm the kernel has actually stopped the process. `ps` reports a stopped
/// process's state starting with `T` (or `t` under a tracer), portably across
/// Linux and macOS, so a signal that was accepted but not yet applied does not
/// read as frozen.
#[cfg(unix)]
fn wait_until_stopped(pid: u32) -> io::Result<()> {
    for _ in 0..50 {
        let out = std::process::Command::new("ps")
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()?;
        let state = String::from_utf8_lossy(&out.stdout);
        let state = state.trim();
        if state.starts_with('T') || state.starts_with('t') {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Err(io::Error::other(format!(
        "process {pid} did not reach a stopped state"
    )))
}

/// The one P/Invoke lives in PowerShell, not in Rust, because this crate forbids
/// `unsafe`. The script suspends or resumes every thread of the target and, for
/// a suspend, confirms the threads actually report `Suspended` before it exits
/// zero -- the ntdll return code alone does not prove the stop took effect.
#[cfg(windows)]
const FREEZE_PS1: &str = r#"param([int]$TargetPid, [string]$Action)
$ErrorActionPreference = 'Stop'
Add-Type -Namespace ScodeFreeze -Name Nt -MemberDefinition @"
[System.Runtime.InteropServices.DllImport("ntdll.dll")] public static extern int NtSuspendProcess(System.IntPtr h);
[System.Runtime.InteropServices.DllImport("ntdll.dll")] public static extern int NtResumeProcess(System.IntPtr h);
"@
$handle = (Get-Process -Id $TargetPid).Handle
if ($Action -eq 'suspend') {
  $rc = [ScodeFreeze.Nt]::NtSuspendProcess($handle)
  if ($rc -ne 0) { Write-Error "NtSuspendProcess rc=$rc"; exit 2 }
  Start-Sleep -Milliseconds 200
  $awake = (Get-Process -Id $TargetPid).Threads | Where-Object { $_.WaitReason -ne 'Suspended' }
  if ($awake) { Write-Error 'threads not all suspended'; exit 3 }
  exit 0
} elseif ($Action -eq 'resume') {
  $rc = [ScodeFreeze.Nt]::NtResumeProcess($handle)
  if ($rc -ne 0) { Write-Error "NtResumeProcess rc=$rc"; exit 2 }
  exit 0
} else {
  Write-Error "unknown action $Action"; exit 1
}
"#;

#[cfg(windows)]
fn freeze_pid(pid: u32) -> io::Result<()> {
    run_powershell(pid, "suspend")
}

#[cfg(windows)]
fn resume_pid(pid: u32) -> io::Result<()> {
    run_powershell(pid, "resume")
}

#[cfg(windows)]
fn run_powershell(pid: u32, action: &str) -> io::Result<()> {
    // A unique path per (test process, target) so parallel tests never read a
    // half-written script, and it is removed as soon as PowerShell has read it.
    let script =
        std::env::temp_dir().join(format!("scode_freeze_{}_{}.ps1", std::process::id(), pid));
    std::fs::write(&script, FREEZE_PS1)?;
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .args(["-TargetPid", &pid.to_string(), "-Action", action])
        .status();
    let _ = std::fs::remove_file(&script);
    if !status?.success() {
        return Err(io::Error::other(format!(
            "powershell {action} for process {pid} did not succeed"
        )));
    }
    Ok(())
}
