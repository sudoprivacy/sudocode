#!/usr/bin/env python3
"""Run the actual release executor with Cargo and distinct native binaries.

Tiny process fixtures prove artifact identity, workspace-driver reuse and failure
propagation. They do not establish product PTY or live-provider coverage.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    profiles = load("release_profiles", SOURCE / ".github/local-acceptance/checks.py")
    helper = load("release_executor", SOURCE / ".github/scripts/local_acceptance.py")
    root, target = output / "project", output / "target"
    rust = root / "rust"
    files = {
        "rust/Cargo.toml": '''[package]
name="rusty-sudocode-cli"
version="0.0.1"
edition="2021"
[workspace]
members=["support", "engine-acp"]
resolver="2"
[[bin]]
name="scode"
path="src/main.rs"
[dependencies]
shared-support={path="support",features=["normal"]}
engine-acp={path="engine-acp"}
[dev-dependencies]
shared-support={path="support",features=["dev"]}
''',
        "rust/src/main.rs": '''fn main() {
    let structured = std::env::args().nth(1).as_deref() == Some("version");
    if std::env::var_os("SCODE_TRACE_STARTUP").is_some() {
        eprintln!("startup phase=console elapsed_us=1 cumulative_us=1");
        eprintln!("startup phase=clap elapsed_us=1 cumulative_us=2");
        if structured { eprintln!("startup phase=version_output elapsed_us=1 cumulative_us=3"); }
    }
    if structured {
        println!(r#"{{"debug":{},"flavor":"{}"}}"#, cfg!(debug_assertions), shared_support::flavor());
    } else {
        println!("debug={},flavor={}", cfg!(debug_assertions), shared_support::flavor());
    }
}
''',
        "rust/support/Cargo.toml": '[package]\nname="shared-support"\nversion="0.0.1"\nedition="2021"\n'
            '[features]\nnormal=[]\ndev=[]\n',
        "rust/support/src/lib.rs": 'pub fn flavor() -> &\'static str { if cfg!(feature="dev") { "dev" } else { "normal" } }\n',
        "rust/engine-acp/Cargo.toml": '[package]\nname="engine-acp"\nversion="0.0.1"\nedition="2021"\n'
            '[features]\nmailbox=[]\n',
        "rust/engine-acp/src/lib.rs": 'pub const MAILBOX: bool = cfg!(feature="mailbox");\n',
        "rust/tests/pty_model_stream_close.rs": '''#[test]
fn raw_stream_capture_keeps_streaming_and_resume_responsive() {
    assert!(cfg!(debug_assertions), "Only the driver uses the test profile");
    assert!(engine_acp::MAILBOX, "Reuse the same native workspace feature graph");
    assert_eq!(shared_support::flavor(), "dev");
    let binary = std::env::var_os("SCODE_TEST_BIN").expect("Explicit release artifact required");
    assert_ne!(std::path::Path::new(&binary), std::path::Path::new(env!("CARGO_BIN_EXE_scode")));
    let output = std::process::Command::new(binary).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "debug=false,flavor=normal");
}
#[test]
fn unselected_case_must_not_run() { panic!("release profile widened its selection"); }
''',
    }
    for relative, contents in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")
    startup = root / "e2e/startup"
    startup.mkdir(parents=True)
    for name in ("run.py", "reference.rs"):
        (startup / name).write_bytes((SOURCE / "e2e/startup" / name).read_bytes())
    environment = {"CARGO_TARGET_DIR": str(target), "SCODE_TEST_BACKEND": "mock"}
    commands = []

    def execute(args, cwd, log, env=None):
        record = helper.run_command(args, cwd, log, {**environment, **(env or {})})
        commands.append(record)
        return record

    def cargo(*args, name):
        return execute(["cargo", *args], rust, output / (name + ".log"))

    cargo("generate-lockfile", name="lockfile")
    cargo("test", "--locked", "--features", "engine-acp/mailbox", "--test", "pty_model_stream_close",
          "--no-run", "--message-format=json-render-diagnostics", name="workspace-driver")
    artifacts = [json.loads(line) for line in (output / "workspace-driver.log").read_text(encoding="utf-8").splitlines()
                 if line.startswith("{")]
    driver = Path(next(row["executable"] for row in artifacts if row.get("reason") == "compiler-artifact"
                       and row.get("target", {}).get("name") == "pty_model_stream_close"))
    before = (driver.stat().st_mtime_ns, hashlib.sha256(driver.read_bytes()).hexdigest())
    previous = os.environ.get("CARGO_TARGET_DIR")
    os.environ["CARGO_TARGET_DIR"] = str(target)
    try:
        records = profiles.run("native-release", root, output / "release", "fixture-base", execute)
    finally:
        if previous is None:
            os.environ.pop("CARGO_TARGET_DIR", None)
        else:
            os.environ["CARGO_TARGET_DIR"] = previous
    assert len(records) == 3, "Release build, exact native driver and startup budget must all execute"
    assert before == (driver.stat().st_mtime_ns, hashlib.sha256(driver.read_bytes()).hexdigest()), "Workspace driver rebuilt"
    driver_log = (output / "release/001.log").read_text(encoding="utf-8")
    assert "1 passed; 0 failed; 0 ignored" in driver_log and "1 filtered out" in driver_log, driver_log
    assert "Compiling " not in driver_log, "The exact workspace driver graph should already be warm"
    suffix = ".exe" if os.name == "nt" else ""
    binary = output / "release" / ("scode" + suffix)
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    assert digest == hashlib.sha256((target / "release" / ("scode" + suffix)).read_bytes()).hexdigest()
    assert subprocess.check_output([str(binary)]).decode().strip() == "debug=false,flavor=normal"
    startup_report = json.loads((output / "release/startup.json").read_text(encoding="utf-8"))
    assert startup_report["binary"] == str(binary.resolve()) and startup_report["binary_bytes"] == binary.stat().st_size
    assert all(row["samples"] == 40 and row["budget_ms"] == 100 for row in startup_report["commands"])
    print("Expected negative: substituting the debug binary must fail the exact release-artifact assertion.", flush=True)
    wrong_binary = output / ("wrong-scode" + suffix)
    shutil.copy2(target / "debug" / ("scode" + suffix), wrong_binary)
    try:
        execute([str(driver), "raw_stream_capture_keeps_streaming_and_resume_responsive", "--exact", "--nocapture"],
                rust, output / "wrong-artifact.log", {"SCODE_TEST_BIN": str(wrong_binary)})
        raise AssertionError("Wrong debug artifact unexpectedly accepted")
    except RuntimeError:
        negative = json.loads((output / "wrong-artifact.command.json").read_text(encoding="utf-8"))
        assert negative["exit_code"] == 101, negative
        assert "debug=true,flavor=dev" in (output / "wrong-artifact.log").read_text(encoding="utf-8")
    (output / "result.json").write_text(json.dumps({
        "status": "PASS", "commands": commands, "release_sha256": digest,
        "driver_sha256": before[1], "startup": startup_report,
        "scope": "Real Cargo/native release executor fixture; not product PTY or provider coverage."}, indent=2), encoding="utf-8")
    print(f"PASS: normal release child, reused native workspace driver, unchanged startup budget and wrong-artifact refusal; {output}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run(args.output.resolve())
