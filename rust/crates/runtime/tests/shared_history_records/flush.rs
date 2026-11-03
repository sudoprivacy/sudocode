//! Injected fdatasync failure in a real process: visible LF is not a commit.
use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use command_group::CommandGroup;
use runtime::fs_backend::{FsBackend, RecordLogMode, StdFsBackend};
use runtime::history::{HistoryLog, HistoryRecord};
use runtime::Session;

use super::{commit, Fixture, Worker, MODE, ROOT};

const TEST: &str = "flush::native_history_resynchronizes_after_failed_flush";
const EFFECT: &str = "effect-after-durable-read";

struct TraceCall {
    started: usize,
    finished: usize,
    call: String,
}

/// Preserve syscall completion order when strace interleaves tracee threads.
fn calls(trace: &str) -> Vec<TraceCall> {
    let mut pending = HashMap::new();
    let mut calls = Vec::new();
    for (index, line) in trace.lines().enumerate() {
        let Some((thread, call)) = line.trim_start().split_once(' ') else {
            continue;
        };
        let call = call.trim_start();
        if let Some(prefix) = call.strip_suffix("<unfinished ...>") {
            pending.insert(thread, (index, prefix.to_owned()));
        } else if let Some(resumed) = call.strip_prefix("<... ") {
            let (name, suffix) = resumed.split_once(" resumed>").expect("resumed syscall");
            let (started, prefix) = pending.remove(thread).expect("unfinished syscall");
            assert!(prefix.starts_with(&format!("{name}(")));
            calls.push(TraceCall {
                started,
                finished: index,
                call: prefix + suffix,
            });
        } else {
            calls.push(TraceCall {
                started: index,
                finished: index,
                call: call.into(),
            });
        }
    }
    assert!(pending.is_empty(), "unfinished syscall at process exit");
    calls
}

#[test]
#[ignore = "Linux strace syscall injection; e2e/nexus-a2a/run.sh runs this explicitly"]
fn native_history_resynchronizes_after_failed_flush() {
    if std::env::var(MODE).as_deref() == Ok("flush-error") {
        let root = PathBuf::from(std::env::var(ROOT).unwrap());
        let raw = StdFsBackend
            .record_log(
                &root.join("history.jsonl").to_string_lossy(),
                RecordLogMode::CreateNew,
            )
            .unwrap();
        let mut log = HistoryLog::new(raw);
        commit(
            &mut log,
            &HistoryRecord::initialize(&Session::new()).unwrap(),
        );
        // This externally visible effect is allowed only after the committed
        // record has been observed durably, including after an ambiguous error.
        std::fs::write(root.join(EFFECT), b"allowed").unwrap();
        return;
    }

    let fixture = Fixture::new();
    let trace_path = fixture.0.join("flush-syscalls.log");
    let output = File::create(fixture.0.join("flush-worker.log")).unwrap();
    let mut worker = Worker(
        Command::new("strace")
            .args(["-f", "-qq", "-yy", "-s", "4096", "-o"])
            .arg(&trace_path)
            .args([
                "-e",
                "trace=fdatasync,openat",
                "-e",
                "inject=fdatasync:error=EIO:when=1",
            ])
            .arg(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", TEST, "--nocapture"])
            .env(ROOT, &fixture.0)
            .env(MODE, "flush-error")
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .group_spawn()
            .expect("strace must be installed for native durability acceptance"),
    );
    worker.finish();
    assert_eq!(std::fs::read(fixture.0.join(EFFECT)).unwrap(), b"allowed");
    let trace = std::fs::read_to_string(trace_path).unwrap();
    let calls = calls(&trace);
    let failed = calls
        .iter()
        .find(|event| {
            event.call.contains("fdatasync(")
                && event.call.contains("history.jsonl")
                && event.call.contains("INJECTED")
        })
        .expect("the append flush must receive the injected EIO");
    let recovered = calls
        .iter()
        .find(|event| {
            event.started > failed.finished
                && event.call.contains("fdatasync(")
                && event.call.contains("history.jsonl")
                && event.call.ends_with("= 0")
        })
        .expect("a visible failed append must be flushed successfully before replay trusts it");
    let effect = calls
        .iter()
        .find(|event| event.call.contains("openat(") && event.call.contains(EFFECT))
        .expect("the child performed its real file effect");
    assert!(
        failed.finished < recovered.started && recovered.finished < effect.started,
        "{trace}"
    );
    let history = std::fs::read_to_string(fixture.0.join("history.jsonl")).unwrap();
    assert_eq!(
        history.lines().count(),
        1,
        "ambiguous retry duplicated its ID"
    );
}
