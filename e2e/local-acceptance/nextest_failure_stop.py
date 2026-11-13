"""Compare failure cancellation using real PTYs and native descendants."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import threading
import time

import psutil


SOURCE = Path(__file__).resolve().parents[2]


def run(helper, profiles, executable, output):
    output.mkdir(parents=True, exist_ok=False)
    fixture = output / "project"
    (fixture / "src").mkdir(parents=True)
    (fixture / "tests").mkdir()
    for source, target in [("compiler-objects.Cargo.toml", "Cargo.toml"),
                           ("compiler-objects.Cargo.lock", "Cargo.lock")]:
        shutil.copyfile(Path(__file__).with_name(source), fixture / target)
    (fixture / "src/main.rs").write_text(r'''use std::{fs, path::Path, process::Command, thread, time::{Duration, Instant}};
fn publish(root: &Path, name: &str) {
    let temporary = root.join(format!("{}.partial", name));
    fs::write(&temporary, std::process::id().to_string()).unwrap();
    fs::rename(temporary, root.join(name)).unwrap();
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let root = Path::new(&args[1]);
    if args.len() > 2 {
        publish(root, "grandchild.pid");
        thread::sleep(Duration::from_secs(6));
        return;
    }
    publish(root, "child.pid");
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.arg(root).arg("leaf");
    #[cfg(unix)] {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().unwrap();
    #[cfg(windows)] let job = unsafe {
        use std::{ffi::c_void, os::windows::io::AsRawHandle};
        #[link(name="kernel32")]
        unsafe extern "system" {
            fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> *mut c_void;
            fn AssignProcessToJobObject(job: *mut c_void, process: *mut c_void) -> i32;
            fn IsProcessInJob(process: *mut c_void, job: *mut c_void, result: *mut i32) -> i32;
        }
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        assert!(!job.is_null());
        assert_ne!(AssignProcessToJobObject(job, child.as_raw_handle()), 0);
        let mut assigned = 0;
        assert_ne!(IsProcessInJob(child.as_raw_handle(), job, &mut assigned), 0);
        assert_ne!(assigned, 0);
        fs::write(root.join("group-proof"), "windows-nested-job").unwrap();
        job
    };
    #[cfg(unix)] unsafe {
        unsafe extern "C" { fn getpgid(pid: i32) -> i32; fn getpgrp() -> i32; }
        assert_eq!(getpgid(child.id() as i32), child.id() as i32);
        assert_ne!(getpgid(child.id() as i32), getpgrp());
        fs::write(root.join("group-proof"), "independent-unix-process-group").unwrap();
    }
    let start = Instant::now();
    while !root.join("grandchild.pid").exists() {
        assert!(start.elapsed() < Duration::from_secs(10));
        thread::sleep(Duration::from_millis(10));
    }
    println!("PTY_FAILURE_STOP_READY");
    assert!(child.wait().unwrap().success());
    #[cfg(windows)] unsafe {
        #[link(name="kernel32")]
        unsafe extern "system" { fn CloseHandle(handle: *mut std::ffi::c_void) -> i32; }
        assert_ne!(CloseHandle(job), 0);
    }
}
''', encoding="utf-8")
    (fixture / "tests/process.rs").write_text(r'''use std::{fs, path::PathBuf, thread, time::{Duration, Instant}};
fn root() -> PathBuf { PathBuf::from(std::env::var_os("FAILURE_STOP_CONTROL").unwrap()) }
#[test]
fn running_pty() {
    let directory = root();
    let temporary = directory.join("test.pid.partial");
    fs::write(&temporary, std::process::id().to_string()).unwrap();
    fs::rename(temporary, directory.join("test.pid")).unwrap();
    let bin = env!("CARGO_BIN_EXE_compiler-object-consumer");
    let argument = directory.to_str().unwrap();
    let mut session = pty_expect::PtySession::spawn(bin, &[argument]).unwrap();
    session.set_default_timeout(Duration::from_secs(15));
    session.expect("PTY_FAILURE_STOP_READY").unwrap();
    fs::write(directory.join("ready"), "ready").unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
    fs::write(directory.join("running-finished"), "finished").unwrap();
}
#[test]
fn original_failure() {
    let directory = root();
    let start = Instant::now();
    while !directory.join("release-failure").exists() {
        assert!(start.elapsed() < Duration::from_secs(10), "fixture barrier timed out");
        thread::sleep(Duration::from_millis(10));
    }
    panic!("deliberate-pty-failure-stop-control");
}
''', encoding="utf-8")
    inputs = {path: hashlib.sha256(path.read_bytes()).hexdigest()
              for path in fixture.rglob("*") if path.is_file()}
    target = output / "target"
    environment = {"CARGO_TARGET_DIR": str(target), "SCODE_ACCEPTANCE_BUILD_JOBS": "1",
                   "CARGO_BUILD_JOBS": "1", "CARGO_INCREMENTAL": "0",
                   "CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0"}
    if os.name == "nt":
        terminal_log = output / "terminal.log"
        command = profiles.windows_terminal_command(SOURCE)
        command[-1] = str(target)
        helper.run_command(command, SOURCE, terminal_log)
        terminal = profiles.verify_native_terminal(terminal_log, target)
    else:
        terminal = None
    helper.run_command(["cargo", "test", "--manifest-path", str(fixture / "Cargo.toml"),
                        "--locked", "--offline", "--no-run"], fixture, output / "build.log", environment)
    suffix = ".exe" if os.name == "nt" else ""
    programs = [p for p in (target / "debug/deps").glob("process-*")
                if p.is_file() and (p.suffix == ".exe" if suffix else p.suffix == "")]
    assert len(programs) == 1, programs
    binary = target / "debug" / ("compiler-object-consumer" + suffix)
    programs = {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in [programs[0], binary]}
    options = profiles.nextest_options(output)
    configured = options[options.index("--max-fail") + 1]
    assert configured == ("1:immediate" if os.name == "nt" else "1:wait"), configured
    options[options.index("--test-threads") + 1] = "2"
    rows = []
    # POSIX shell tools intentionally use separate process groups. Retain the
    # original wait behavior there; a nextest process-group signal alone is
    # insufficient to qualify their descendants' retirement.
    for mode in (("wait", "immediate") if os.name == "nt" else ("wait",)):
        directory = output / mode
        directory.mkdir()
        row = {"mode": mode, "identities": [], "observer_error": None, "released_at": None}
        completed = threading.Event()

        def observe():
            try:
                deadline = time.monotonic() + 30
                while not (directory / "ready").exists():
                    if completed.wait(.01) or time.monotonic() > deadline:
                        raise RuntimeError("PTY control ended before its process barrier")
                for name, expected in [("test.pid", next(iter(programs))),
                                       ("child.pid", binary), ("grandchild.pid", binary)]:
                    process = psutil.Process(int((directory / name).read_text()))
                    assert Path(process.exe()).resolve() == expected.resolve()
                    if os.name == "nt":
                        assert process.nice() == psutil.NORMAL_PRIORITY_CLASS
                    else:
                        assert process.nice() == 0
                    row["identities"].append({"pid": process.pid, "birth": process.create_time(),
                                              "exe": str(expected), "priority": process.nice()})
                row["process_group_proof"] = (directory / "group-proof").read_text()
                assert row["process_group_proof"] == (
                    "windows-nested-job" if os.name == "nt" else "independent-unix-process-group")
                # The real ConPTY host also belongs to this private test tree.
                if terminal:
                    allowed = target / "debug/deps/OpenConsole.exe"
                    for process in psutil.Process(row["identities"][0]["pid"]).children(recursive=True):
                        if Path(process.exe()).resolve() == allowed.resolve():
                            row["identities"].append({"pid": process.pid, "birth": process.create_time(),
                                                      "exe": str(allowed), "priority": process.nice()})
                    assert any(Path(p["exe"]).name == "OpenConsole.exe" for p in row["identities"])
                row["released_at"] = time.monotonic()
                (directory / "release-failure").write_text("release original failure")
            except BaseException as error:
                row["observer_error"] = repr(error)

        observer = threading.Thread(target=observe)
        observer.start()
        current = list(options)
        current[current.index("--max-fail") + 1] = "1:" + mode
        log = directory / "nextest.log"
        try:
            helper.run_command([str(executable), "nextest", "run", *current, "--manifest-path",
                                str(fixture / "Cargo.toml"), "--locked", "--offline"], fixture, log,
                               {**environment, "FAILURE_STOP_CONTROL": str(directory)})
        except RuntimeError:
            pass
        finally:
            stopped = time.monotonic()
            completed.set()
            observer.join(timeout=2)
            row["command"] = json.loads(log.with_suffix(".command.json").read_text())
            row["failure_stop_seconds"] = stopped - row["released_at"] if row["released_at"] else None
            row["finished_naturally"] = (directory / "running-finished").exists()
            row["remaining_processes"] = []
            for identity in row["identities"]:
                try:
                    process = psutil.Process(identity["pid"])
                    if process.create_time() == identity["birth"] and process.status() != psutil.STATUS_ZOMBIE:
                        assert Path(process.exe()).resolve() == Path(identity["exe"]).resolve()
                        row["remaining_processes"].append(identity)
                        process.terminate()
                        process.wait(timeout=5)
                except psutil.NoSuchProcess:
                    pass
            rows.append(row)
            (output / "receipt.json").write_text(json.dumps({"rows": rows, "terminal": terminal}, indent=2))
        assert not observer.is_alive() and row["observer_error"] is None, row
        assert row["command"]["exit_code"] == 100, row
        assert "deliberate-pty-failure-stop-control" in log.read_text(encoding="utf-8"), row
        assert row["finished_naturally"] == (mode == "wait"), row
        assert not row["remaining_processes"], row
        assert all(hashlib.sha256(path.read_bytes()).hexdigest() == digest for path, digest in programs.items())
    if os.name == "nt":
        assert rows[1]["failure_stop_seconds"] < rows[0]["failure_stop_seconds"], rows
    assert all(hashlib.sha256(path.read_bytes()).hexdigest() == digest for path, digest in inputs.items())
    result = {"status": "PASS", "rows": rows, "terminal": terminal,
              "inputs": {str(p): h for p, h in inputs.items()},
              "programs": {str(p): h for p, h in programs.items()}}
    (output / "receipt.json").write_text(json.dumps(result, indent=2))
    return result
