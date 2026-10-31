//! Actual process stderr, OS pipe backpressure and the production forwarder.
//! The printer is gated to control scheduling; native input preservation stays
//! covered by `pty_render_boundaries` and `pty_sync_repl_receive`.

use std::time::Duration;

#[allow(dead_code)]
#[path = "../src/render"]
mod render {
    #[path = "output.rs"]
    pub(crate) mod output;
}
#[path = "../src/stderr_capture.rs"]
mod stderr_capture;

#[path = "../src/input"]
mod input {
    #[path = "stderr.rs"]
    mod stderr;

    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum Writer {
        Retained,
        Global,
        Diagnostics,
    }

    struct GatedPrinter {
        entered: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
        log: PathBuf,
        expired: Arc<AtomicBool>,
    }

    impl rustyline::ExternalPrinter for GatedPrinter {
        fn print(&mut self, _text: String) -> rustyline::Result<()> {
            if let Some(entered) = self.entered.take() {
                stage(&self.log, "printer gated");
                entered.send(()).unwrap();
                // Release even on a regression so a panic need not write into
                // the same full pipe. The timeout is asserted after cleanup.
                if self.release.recv_timeout(Duration::from_secs(5)).is_err() {
                    self.expired.store(true, Ordering::Release);
                    stage(&self.log, "printer gate expired");
                }
            }
            Ok(())
        }
    }

    #[inline]
    fn stage(path: &Path, text: &str) {
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(log, "{text}").unwrap();
    }

    /// A retained library handle must stay on its old pipe across dup2 or
    /// `SetStdHandle`. A global stderr write can instead retry on its restored
    /// target, or fail on Darwin when dup2 cancels its in-flight fd-2 write.
    #[cfg(unix)]
    #[inline]
    fn retained_stderr() -> std::fs::File {
        use std::os::fd::AsFd;
        std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .unwrap()
            .into()
    }

    #[cfg(windows)]
    #[inline]
    fn retained_stderr() -> std::fs::File {
        filedescriptor::FileDescriptor::dup(&std::io::stderr())
            .unwrap()
            .as_file()
            .unwrap()
    }

    fn write_blocked_stderr(
        log: &Path,
        kind: Writer,
        retained: &mut Option<std::fs::File>,
        locked: &mpsc::Sender<()>,
        finished: &mpsc::Sender<std::io::Result<()>>,
    ) {
        let mut stderr = std::io::stderr().lock();
        stage(log, "writer locked stderr");
        locked.send(()).unwrap();
        // Larger than the native pipe plus the Windows capture queue.
        // With the forwarder gated, this write must remain in flight.
        let bytes = vec![b'x'; 8 * 1024 * 1024];
        let result = match kind {
            Writer::Retained => retained.as_mut().unwrap().write_all(&bytes),
            Writer::Global => stderr.write_all(&bytes),
            Writer::Diagnostics => {
                // Force a notice through the same real pipe boundary as
                // the raw writer. This leaked test-only text lives only in
                // this child; production notices remain short constants.
                let pending = Box::leak("x".repeat(bytes.len()).into_boxed_str());
                let diagnostics = telemetry::RetryDiagnostics::new(
                    "capture-fixture",
                    "reader",
                    pending,
                    "checks recovered",
                );
                diagnostics.failed("first", "ORIGINAL_READER_TRANSPORT_ERROR");
                diagnostics.succeeded("first");
                // The original attempt and its recovery must survive even
                // when the terminal notice failed partway through. A
                // second scope proves the observer thread kept running.
                diagnostics.failed("second", "NEXT_READER_CHECK_CONTINUED");
                diagnostics.succeeded("second");
                Ok(())
            }
        };
        drop(stderr);
        // Never panic/report an IO error while owning the global stderr
        // lock. Preserve the original result even if cleanup later stalls.
        stage(log, &format!("writer completed: {result:?}"));
        let _ = finished.send(result);
    }

    pub(super) fn exercise_rearm(log: &Path, kind: Writer) {
        const BUDGET: Duration = Duration::from_secs(5);
        let (entered, printing) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let expired = Arc::new(AtomicBool::new(false));
        let output = Arc::new(
            stderr::StderrOutput::activate(GatedPrinter {
                entered: Some(entered),
                release: released,
                log: log.to_path_buf(),
                expired: Arc::clone(&expired),
            })
            .expect("activate actual process capture"),
        );
        stage(log, "capture created");
        output.set_active(true).unwrap();
        stage(log, "capture active");
        output.printer().print("hold the forwarder".into()).unwrap();
        printing.recv_timeout(BUDGET).unwrap();

        let mut retained = (kind == Writer::Retained).then(retained_stderr);
        let (locked, writing) = mpsc::channel();
        let (finished, written) = mpsc::channel();
        let writer_log = log.to_path_buf();
        let writer = std::thread::spawn(move || {
            write_blocked_stderr(&writer_log, kind, &mut retained, &locked, &finished);
        });
        writing.recv_timeout(BUDGET).unwrap();
        let early_write = written.recv_timeout(Duration::from_millis(100));
        let was_blocked = matches!(early_write, Err(mpsc::RecvTimeoutError::Timeout));
        #[cfg(unix)]
        {
            use std::os::fd::AsFd;
            // Confirm kernel backpressure, rather than inferring a full pipe
            // from a thread which might simply not have been scheduled yet.
            let stderr = std::io::stderr();
            let mut descriptors = [nix::poll::PollFd::new(
                stderr.as_fd(),
                nix::poll::PollFlags::POLLOUT,
            )];
            let deadline = std::time::Instant::now() + BUDGET;
            while nix::poll::poll(&mut descriptors, 0u16).unwrap() != 0 {
                assert!(std::time::Instant::now() < deadline, "pipe never filled");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        // Suspension must not wait for a pipe writer that needs this caller
        // to release the printer gate.
        stage(log, "suspend begin");
        let suspended = output.set_active(false);
        stage(log, &format!("suspend result: {suspended:?}"));
        stage(log, "suspend completed");
        let (starting, started) = mpsc::channel();
        let (rearmed, ready) = mpsc::channel();
        let next = Arc::clone(&output);
        let reader_log = log.to_path_buf();
        let reader = std::thread::spawn(move || {
            starting.send(()).unwrap();
            stage(&reader_log, "rearm begin");
            let result = next.set_active(true);
            stage(&reader_log, &format!("rearm result: {result:?}"));
            stage(&reader_log, "rearm completed");
            let _ = rearmed.send(result);
        });
        started.recv_timeout(BUDGET).unwrap();
        let early_rearm = ready.recv_timeout(Duration::from_millis(100));
        let rearm_waited = matches!(early_rearm, Err(mpsc::RecvTimeoutError::Timeout));
        // Rearming must leave the redirect mutex available so this forwarder
        // can drain the old pipe and let the writer release stderr.
        let released = release.send(());
        stage(log, "printer released");
        let written = match early_write {
            Err(mpsc::RecvTimeoutError::Timeout) => written.recv_timeout(BUDGET),
            result => result,
        };
        let ready = match early_rearm {
            Err(mpsc::RecvTimeoutError::Timeout) => ready.recv_timeout(BUDGET),
            result => result,
        };
        writer.join().unwrap();
        reader.join().unwrap();
        output.set_active(false).unwrap();
        drop(output);
        stage(log, "capture dropped");
        assert!(!expired.load(Ordering::Acquire), "printer gate expired");
        assert!(was_blocked, "write did not reach pipe backpressure");
        if kind == Writer::Retained {
            assert!(rearm_waited, "rearm bypassed the in-flight stderr writer");
        }
        suspended.expect("suspend with an in-flight writer");
        released.expect("release the gated printer");
        let written = written.expect("drain blocked writer");
        // Darwin cancels an in-flight write to fd 2 when dup2 replaces it,
        // even while other descriptors keep both pipe ends alive. The
        // retained-handle case still requires lossless IO. The diagnostics
        // case separately requires the actual observer to survive this error
        // and retain both original failures and recoveries in its local log.
        if kind == Writer::Global && cfg!(target_os = "macos") {
            let error = written.expect_err("Darwin must expose interrupted fd-2 IO");
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            println!("DARWIN_FD2_HANDOFF_BROKEN_PIPE");
        } else {
            written.expect("retained write or diagnostics observer must finish");
        }
        ready
            .expect("rearm after pipe drain")
            .expect("rearm capture");
        println!("CAPTURE_REARM_COMPLETED");
    }
}

#[tokio::test]
async fn a_full_capture_pipe_can_drain_while_the_next_prompt_rearms() {
    run_capture_case(
        "a_full_capture_pipe_can_drain_while_the_next_prompt_rearms",
        input::Writer::Retained,
    )
    .await;
}

#[tokio::test]
async fn a_global_stderr_write_exposes_its_actual_handoff_outcome() {
    run_capture_case(
        "a_global_stderr_write_exposes_its_actual_handoff_outcome",
        input::Writer::Global,
    )
    .await;
}

#[tokio::test]
async fn a_retry_notice_cannot_abort_its_observer_during_terminal_handoff() {
    run_capture_case(
        "a_retry_notice_cannot_abort_its_observer_during_terminal_handoff",
        input::Writer::Diagnostics,
    )
    .await;
}

async fn run_capture_case(test: &str, kind: input::Writer) {
    const CHILD: &str = "SCODE_CAPTURE_REARM_FIXTURE";
    if let Some(log) = std::env::var_os(CHILD) {
        input::exercise_rearm(std::path::Path::new(&log), kind);
        return;
    }
    // Informational layouts from the actual compiler/target. These do not
    // measure retained states or attribute the rendering workload's RSS.
    println!(
        "layout bytes: CompletionState={} RetryDiagnostics={} inlineOnceLock={} boxedOnceLock={}",
        std::mem::size_of::<engine_core::subagent_completion::CompletionState>(),
        std::mem::size_of::<telemetry::RetryDiagnostics>(),
        std::mem::size_of::<std::sync::OnceLock<telemetry::RetryDiagnostics>>(),
        std::mem::size_of::<std::sync::OnceLock<Box<telemetry::RetryDiagnostics>>>(),
    );
    // Keep process-global stderr replacement out of the parallel test runner.
    // Kill a deadlocked child on timeout instead of hanging the workspace job.
    let files = tempfile::tempdir().unwrap();
    let log = files.path().join("handoff.log");
    let diagnostic_log = files.path().join("diagnostics.log");
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, &log)
            .env("SCODE_LOG_PATH", &diagnostic_log)
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let stages = std::fs::read_to_string(&log).unwrap_or_default();
    let result = result
        .unwrap_or_else(|_| panic!("capture handoff must not deadlock; stages:\n{stages}"))
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = &result.stderr[result.stderr.len().saturating_sub(8192)..];
    assert!(
        result.status.success() && stdout.contains("CAPTURE_REARM_COMPLETED"),
        "stages:\n{stages}\n{stdout}\n{}",
        String::from_utf8_lossy(stderr)
    );
    if kind == input::Writer::Diagnostics {
        let original = std::fs::read_to_string(diagnostic_log).unwrap();
        let records = original
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        for (scope, error) in [
            ("first", "ORIGINAL_READER_TRANSPORT_ERROR"),
            ("second", "NEXT_READER_CHECK_CONTINUED"),
        ] {
            for action in ["background_retry", "background_retry_scope_recovered"] {
                assert!(
                    records.iter().any(|record| {
                        let attributes = &record["attributes"];
                        attributes["action"] == action
                            && attributes["scope"] == scope
                            && attributes["error"] == error
                    }),
                    "missing {action}/{scope}: {original}"
                );
            }
        }
        if cfg!(target_os = "macos") {
            let failed = records
                .iter()
                .filter(|record| {
                    record["attributes"]["action"] == "background_retry_terminal_unavailable"
                })
                .collect::<Vec<_>>();
            assert_eq!(
                failed.len(),
                1,
                "must cover the actual failed notice: {original}"
            );
            assert_eq!(failed[0]["attributes"]["context"]["kind"], "BrokenPipe");
        }
    }
}
