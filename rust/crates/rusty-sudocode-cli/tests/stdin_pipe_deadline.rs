//! `--print` must not block forever on an inherited stdin pipe.
//!
//! The bug: a parent spawns `scode` without deciding what stdin should be, so
//! the child inherits a pipe nobody ever writes to and nobody ever closes —
//! what a harness, a CI step or a background job looks like. `read_to_string`
//! on that pipe never returns, and the CLI does nothing at all: one thread, no
//! CPU, no network, no request ever built. It was observed parked for 50
//! minutes.
//!
//! These are integration tests rather than unit tests on purpose, and not PTY
//! tests either:
//!
//!   * a unit test can only inject a fake readiness predicate, which proves the
//!     surrounding logic and *nothing about either platform's readiness check* —
//!     precisely the half where the bug lived;
//!   * a PTY test hands the child a terminal, and `read_piped_stdin` returns
//!     early on `is_terminal()`, so the piped path is never reached.
//!
//! Driving the real binary with a real pipe is the only shape that can fail
//! when the platform check is wrong, and it runs natively on Linux, macOS and
//! Windows in CI.

use std::io::{BufRead, BufReader};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{mpsc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Serialises child spawning across the tests in this binary.
///
/// cargo runs these tests in parallel threads of ONE process, and creating a
/// pipe is not atomic with marking it close-on-exec: macOS has no `pipe2`, so
/// std does `pipe()` then `fcntl(FD_CLOEXEC)`. A spawn from another thread
/// inside that window snapshots the file-descriptor table with the flag unset,
/// and that child inherits this test's stdin write end — keeping the pipe open
/// for its whole life. The child under test then sees no POLLIN and no POLLHUP,
/// burns its 3s first-byte deadline, and emits the very warning this file
/// asserts must not appear.
///
/// Holding this across spawn AND the disposition of the write end is what makes
/// "the only writer is gone" true at the moment the child looks.
fn spawn_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Generous: the assertion is "finite, not 50 minutes". Binary startup on a
/// cold CI runner dwarfs the 3s first-byte deadline being exercised.
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(60);

const WARNING_FRAGMENT: &str = "no stdin data received";

/// Spawn `--print` with stdin as a pipe, and stream stderr lines back.
///
/// `hold_stdin_open` is the whole experiment: `true` reproduces the inherited
/// pipe that never delivers, `false` is the ordinary `printf '' |` case.
/// Returns the child, its stderr lines, and — when `hold_stdin_open` — the
/// write end, kept ALIVE by the caller rather than leaked.
///
/// `std::mem::forget` held it open by never closing it at all, which also left
/// it open for every later spawn in the process. Handing it back scopes it to
/// the test that wants it.
fn spawn_with_pipe(
    label: &str,
    hold_stdin_open: bool,
) -> (
    std::process::Child,
    mpsc::Receiver<String>,
    Option<ChildStdin>,
) {
    let config_home = std::env::temp_dir().join(format!(
        "scode-stdin-deadline-{label}-{}",
        std::process::id()
    ));

    // Held across spawn + the write-end disposition below; see `spawn_lock`.
    let guard = spawn_lock();
    let mut child = Command::new(env!("CARGO_BIN_EXE_scode"))
        // An empty config home keeps the test off the developer's real
        // credentials. The run is expected to fail once it reaches the API;
        // everything asserted here happens strictly before that.
        .env("SUDO_CODE_CONFIG_HOME", &config_home)
        .args([
            "--print",
            // read_piped_stdin() is only consulted in this mode: the other
            // modes keep stdin free for interactive permission prompts.
            "--permission-mode",
            "danger-full-access",
            "hello",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("scode should spawn");

    let stdin = child.stdin.take().expect("stdin should be piped");
    let held = if hold_stdin_open {
        // Never written; held open for exactly as long as the caller keeps it.
        Some(stdin)
    } else {
        // Closed before any other thread may spawn, so the child's first look
        // at the pipe finds EOF rather than an inherited writer.
        drop(stdin);
        None
    };
    drop(guard);

    let stderr = child.stderr.take().expect("stderr should be piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    (child, rx, held)
}

fn wait_for_warning(rx: &mpsc::Receiver<String>, deadline: Duration) -> Option<String> {
    let started = Instant::now();
    while started.elapsed() < deadline {
        let left = deadline.saturating_sub(started.elapsed());
        match rx.recv_timeout(left.min(Duration::from_secs(5))) {
            Ok(line) if line.contains(WARNING_FRAGMENT) => return Some(line),
            // Any other stderr line, or a quiet interval: keep waiting.
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
    None
}

#[test]
fn print_does_not_block_on_a_pipe_that_never_delivers() {
    // given: stdin is a pipe held open with nothing ever written to it
    let (mut child, rx, held_stdin) = spawn_with_pipe("open", true);

    // when
    let warning = wait_for_warning(&rx, OBSERVE_TIMEOUT);
    let _ = child.kill();
    let _ = child.wait();
    drop(held_stdin);

    // then: the deadline was served and the run moved on, instead of parking
    assert!(
        warning.is_some(),
        "--print blocked on a pipe that never delivers: no `{WARNING_FRAGMENT}` \
         warning within {}s. The platform readiness check is reporting a silent \
         pipe as readable, so the CLI started a read it can never finish.",
        OBSERVE_TIMEOUT.as_secs()
    );
}

#[test]
fn print_does_not_warn_when_the_writer_closes_immediately() {
    // given: `printf '' | scode --print ...` — a pipe that is closed at once
    let (mut child, rx, _no_stdin) = spawn_with_pipe("closed", false);

    // when
    let warning = wait_for_warning(&rx, OBSERVE_TIMEOUT);
    let _ = child.kill();
    let _ = child.wait();

    // then: EOF is an answer, not silence. Warning here would mean the check
    // conflates "closed" with "nothing coming" and burns the deadline on every
    // ordinary piped run.
    assert!(
        warning.is_none(),
        "closed-pipe stdin produced the slow-producer warning: {warning:?}"
    );
}
