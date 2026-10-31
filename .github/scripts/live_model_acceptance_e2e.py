#!/usr/bin/env python3
"""Exercise manual-only model policy and the real Cargo supervisor without a key.

The tiny native fixture proves selection, process execution, failure retention
and empty-selection refusal. It does not establish product/model acceptance.
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import time

import psutil


SOURCE = Path(__file__).resolve().parents[2]


def supervisor_cleanup(output):
    """Stop an owned detached child while preserving an unrelated native process."""
    source = SOURCE / ".github/scripts/live_model_acceptance.py"
    spec = importlib.util.spec_from_file_location("live_native_supervisor", source)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    ready = output / "detached-child.json"
    program = ("import subprocess,sys,json,pathlib; "
               "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(120)'],start_new_session=(sys.platform!='win32')); "
               "pathlib.Path(sys.argv[1]).write_text(json.dumps({'pid':child.pid})); child.wait()")
    unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"], start_new_session=os.name != "nt")
    process = subprocess.Popen([sys.executable, "-c", program, str(ready)], start_new_session=os.name != "nt")
    child = None
    try:
        deadline = time.monotonic() + 45
        while not ready.exists() and time.monotonic() < deadline:
            assert process.poll() is None
            time.sleep(.05)
        assert ready.exists(), "Native detached-child fixture did not become ready"
        identifier = json.loads(ready.read_text())["pid"]
        child = psutil.Process(identifier)
        created = child.create_time()
        assert child.ppid() == process.pid
        if os.name != "nt":
            assert os.getpgid(child.pid) != os.getpgid(process.pid)
        module.stop_process(process)
        assert process.poll() is not None
        try:
            assert child.create_time() != created or child.status() == psutil.STATUS_ZOMBIE
        except psutil.NoSuchProcess:
            pass
        assert unrelated.poll() is None, "Do not terminate a process outside the owned tree"
        print("PASS: actual detached native descendant stopped; unrelated native process preserved")
    finally:
        if process.poll() is None:
            module.stop_process(process)
        if child is not None:
            try:
                if child.is_running() and child.status() != psutil.STATUS_ZOMBIE:
                    child.kill()
            except psutil.NoSuchProcess:
                pass
        unrelated.terminate()
        unrelated.wait(timeout=10)


def run(output):
    output.mkdir(parents=True)
    supervisor_cleanup(output)
    repository = output / "repository"
    repository.mkdir()
    environment = {key: value for key, value in os.environ.items()
                   if key not in {"PROXY_AUTH_TOKEN", "ANTHROPIC_API_KEY", "OPENAI_API_KEY",
                                  "NATIVE_PROXY_AUTH_TOKEN", "AZURE_PROXY_AUTH_TOKEN", "SUDOROUTER_API_KEY"}}
    for name in subprocess.check_output(["git", "rev-parse", "--local-env-vars"], cwd=SOURCE, text=True).splitlines():
        environment.pop(name, None)
    environment.update(CARGO_TARGET_DIR=str(output / "target"), CARGO_BUILD_JOBS="1", CARGO_INCREMENTAL="0")

    def command(*arguments, success=True, extra=None):
        result = subprocess.run(list(map(str, arguments)), cwd=repository, env={**environment, **(extra or {})},
                                capture_output=True, text=True, encoding="utf-8", errors="replace")
        assert (result.returncode == 0) == success, (arguments, result.stdout, result.stderr)
        return result

    for name in [".github/scripts/live_model_acceptance.py", ".github/scripts/local_acceptance.py"]:
        target = repository / name
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(SOURCE / name, target)
    for path in ["rust/tests", "rust/src"]:
        (repository / path).mkdir(parents=True, exist_ok=True)
    (repository / "rust/Cargo.toml").write_text('[package]\nname="rusty-sudocode-cli"\nversion="0.0.0"\nedition="2021"\n')
    (repository / "rust/src/lib.rs").write_text("pub fn fixture() {}\n")
    fixture = repository / "rust/tests/pty_headless.rs"
    fixture.write_text(r'''
fn record(value: &str) {
    use std::io::Write;
    let path = std::env::var("MODEL_FIXTURE_RECORD").unwrap();
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
    writeln!(file, "{value}").unwrap();
}
#[test]
fn interactive_file_reads_updated_contents_after_resume() {
    record("first");
    if let Ok(reason) = std::env::var("MODEL_FIXTURE_FAILURE") { panic!("{reason}"); }
}
#[test]
fn print_completes_tool_roundtrip_in_each_output_format() { record("second"); }
''')
    command("git", "init", "--initial-branch=main")
    command("git", "config", "user.name", "Model acceptance fixture")
    command("git", "config", "user.email", "fixture@example.invalid")
    command("cargo", "generate-lockfile", "--offline", "--manifest-path", "rust/Cargo.toml")
    command("git", "add", ".")
    command("git", "commit", "-m", "Native runner fixture")
    helper = repository / ".github/scripts/live_model_acceptance.py"
    marker = output / "executions.txt"
    environment["MODEL_FIXTURE_RECORD"] = str(marker)
    preview = json.loads(command(sys.executable, helper, "--suite", "smoke").stdout)
    assert not preview["paid_calls_started"] and not marker.exists()
    assert len(preview["commands"]) == 2
    command(sys.executable, helper, "--suite", "smoke", "--tests", "unknown_target", "--execute", success=False)
    command(sys.executable, helper, "--suite", "acp", "--execute", success=False)
    assert not marker.exists(), "A preview, unknown target or missing key must not start Cargo"

    good = output / "passed"
    command(sys.executable, helper, "--suite", "smoke", "--execute", "--output", good)
    report = json.loads((good / "result.json").read_text())
    assert report["outcome"] == "passed" and len(report["commands"]) == 2
    assert marker.read_text() == "first\nsecond\n"
    executions = [json.loads(path.read_text()) for path in sorted(good.glob("*.execution.json"))]
    assert len(executions) == 2 and all(record["exit_code"] == 0 for record in executions)
    assert any(record["compiler_priorities"] for record in executions), "Observe the actual native fixture compiler"
    marker.unlink()
    failures = []
    for reason, category in [("upstream quota exceeded", "quota"), ("401 Unauthorized", "authentication"),
                             ("429 Too Many Requests", "rate_limit"), ("503 Service Unavailable", "upstream_unavailable")]:
        destination = output / category
        command(sys.executable, helper, "--suite", "smoke", "--execute", "--output", destination,
                success=False, extra={"MODEL_FIXTURE_FAILURE": reason})
        report = json.loads((destination / "result.json").read_text())
        assert report["outcome"] == "failed" and report["failure_kind"] == category
        assert report["attempts_per_command"] == 1 and len(report["commands"]) == 1
        assert marker.read_text() == "first\n", "Failed work must not retry or admit the next command"
        marker.unlink()
        failures.append(category)
    fixture.write_text(fixture.read_text().replace("fn interactive_file_reads_updated_contents_after_resume()",
                                                 "fn deliberately_not_selected()"))
    empty = output / "empty-selection"
    command(sys.executable, helper, "--suite", "smoke", "--execute", "--output", empty, success=False)
    assert json.loads((empty / "result.json").read_text())["failure_kind"] == "empty_test_selection"
    assert not marker.exists()

    for workflow in ["model-compat.yml", "subagent-parity-live.yml"]:
        text = (SOURCE / ".github/workflows" / workflow).read_text()
        events = text.split("\non:\n", 1)[1].split("\nconcurrency:", 1)[0]
        assert re.findall(r"^  ([a-z_]+):", events, re.M) == ["workflow_dispatch"], workflow
    workflow = (SOURCE / ".github/workflows/rust-ci.yml").read_text()
    assert "if: &paid_model_acceptance" in workflow and "if: *paid_model_acceptance" in workflow
    assert "github.event_name == 'workflow_dispatch' && inputs.live_only && !inputs.cache_only" in workflow
    assert "cargo test -p rusty-sudocode-cli --test pty_step_budget" not in workflow
    for suite in ["smoke", "pty", "acp", "native-acp", "mailbox", "offline-nexus", "subagent", "compat"]:
        plan = json.loads(command(sys.executable, helper, "--suite", suite).stdout)
        assert plan["commands"] and not plan["paid_calls_started"]
        for entry in plan["commands"]:
            arguments = entry["command"]
            if "--test" not in arguments:
                continue
            package = arguments[arguments.index("-p") + 1]
            target = arguments[arguments.index("--test") + 1]
            source = SOURCE / "rust/crates" / package / "tests" / (target + ".rs")
            assert source.is_file(), source
            if "--exact" in arguments:
                case = arguments[arguments.index("--test") + 2]
                assert re.search(r"\bfn " + re.escape(case) + r"\(", source.read_text()), (suite, case)
    (output / "result.json").write_text(json.dumps({"outcome": "passed", "paid_calls": 0,
        "scope": "Real Git/Cargo process and manual trigger enforcement; no product/provider acceptance claim",
        "explicit_failures_retained": failures, "empty_selection_rejected": True,
        "local_and_Actions_command_registry_shared": True}, indent=2) + "\n")
    print("PASS: real native execution, four retained failure classes, no retries, zero-selection refusal and manual-only workflows")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    run(parser.parse_args().output.resolve())
