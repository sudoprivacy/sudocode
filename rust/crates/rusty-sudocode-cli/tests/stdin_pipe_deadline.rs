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
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Generous: the assertion is "finite, not 50 minutes". Binary startup on a
/// cold CI runner dwarfs the 3s first-byte deadline being exercised.
const OBSERVE_TIMEOUT: Duration = Duration::from_secs(60);

const WARNING_FRAGMENT: &str = "no stdin data received";

/// Spawn `--print` with stdin as a pipe, and stream stderr lines back.
///
/// `hold_stdin_open` is the whole experiment: `true` reproduces the inherited
/// pipe that never delivers, `false` is the ordinary `printf '' |` case.
fn spawn_with_pipe(
    label: &str,
    hold_stdin_open: bool,
) -> (std::process::Child, mpsc::Receiver<String>) {
    let config_home = std::env::temp_dir().join(format!(
        "scode-stdin-deadline-{label}-{}",
        std::process::id()
    ));

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
    if hold_stdin_open {
        // Never written, never closed — leaked deliberately, for exactly as
        // long as the child lives.
        std::mem::forget(stdin);
    } else {
        drop(stdin);
    }

    let stderr = child.stderr.take().expect("stderr should be piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    (child, rx)
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
    let (mut child, rx) = spawn_with_pipe("open", true);

    // when
    let warning = wait_for_warning(&rx, OBSERVE_TIMEOUT);
    let _ = child.kill();
    let _ = child.wait();

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
    let (mut child, rx) = spawn_with_pipe("closed", false);

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
