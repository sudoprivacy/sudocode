#!/usr/bin/env python3
"""Exercise the pinned runner with real Cargo builds and native child processes.

These are execution/enforcement checks, not product or live-model coverage.
The intentionally failing child and empty selection must never return success.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--repository", type=Path, default=SOURCE)
    parser.add_argument("--executable", type=Path)
    parser.add_argument("--source-receipt", type=Path)
    parser.add_argument("--options")
    args = parser.parse_args()
    output = (args.output or (Path.home() / ".nexus/local-acceptance/nextest" / str(time.time_ns()))).resolve()
    repository = args.repository.resolve()
    if output == repository or repository in output.parents or output == SOURCE or SOURCE in output.parents:
        raise RuntimeError("Keep nextest acceptance outputs outside the source checkout")
    output.mkdir(parents=True, exist_ok=False)
    helper = load("acceptance_executor", SOURCE / ".github/scripts/local_acceptance.py")
    # Toolchain discovery must read the candidate repository configuration.
    helper.ROOT = repository
    if args.executable is None:
        if args.source_receipt is not None or args.options is not None:
            raise RuntimeError("Supply the executable, source receipt and options together")
        profiles = load("acceptance_profiles", SOURCE / ".github/local-acceptance/checks.py")
        executable = profiles.nextest_binary(output)
        source_path = output / "nextest-source.json"
        options = profiles.nextest_options(output)
    else:
        if args.source_receipt is None or args.options is None:
            raise RuntimeError("Supply the executable, source receipt and options together")
        executable, source_path = args.executable.resolve(), args.source_receipt.resolve()
        options = json.loads(args.options)
        if not isinstance(options, list) or not all(isinstance(option, str) for option in options):
            raise RuntimeError("Native runner options must be a string array")
    source_receipt = json.loads(source_path.read_text())
    if hashlib.sha256(executable.read_bytes()).hexdigest() != source_receipt["executable_sha256"]:
        raise RuntimeError("Native runner differs from its verified source receipt")
    version = subprocess.check_output([str(executable), "nextest", "--version"], text=True).strip()
    if not version.startswith(f"cargo-nextest {source_receipt['version']} "):
        raise RuntimeError(f"Unexpected verified nextest version: {version}")
    fixture = output / "project"
    (fixture / "src").mkdir(parents=True)
    (fixture / "tests").mkdir()
    (fixture / "Cargo.toml").write_text(
        '[package]\nname="nextest-executor-fixture"\nversion="0.0.0"\nedition="2021"\n[workspace]\n')
    (fixture / "Cargo.lock").write_text(
        'version = 4\n\n[[package]]\nname = "nextest-executor-fixture"\nversion = "0.0.0"\n')
    source = fixture / "src/main.rs"
    source.write_text('fn main() { println!("native-child-receipt"); }\n')
    (fixture / "tests/process.rs").write_text('''#[test]
fn actual_binary_is_executed() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_nextest-executor-fixture")).output().unwrap();
    assert!(output.status.success(), "native child failed: {:?}", output.status.code());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "native-child-receipt");
}
#[test]
#[ignore]
fn ignored_case_is_never_executed() { panic!("ignored negative guard executed"); }
''')
    os.environ.pop("NO_COLOR", None)
    os.environ["TERM"] = "xterm-256color"
    arguments = [str(executable), "nextest", "run", *options,
                 "--manifest-path", str(fixture / "Cargo.toml"), "--locked"]
    environment = {"CARGO_TARGET_DIR": str(output / "target")}
    checks = []
    result = helper.run_command(arguments, fixture, output / "native-process.log", environment)
    assert "1 passed, 1 skipped" in (output / "native-process.log").read_text(encoding="utf-8", errors="replace")
    checks.append({"scenario": "native child and ignored guard", "status": "pass", "command": result})
    source.write_text("fn main() { std::process::exit(42); }\n")
    for name, extra, expected in [("native-failure", [], 100),
                                   ("no-matching-cases", ["-E", "test(=missing_case)"], 4)]:
        print(f"Expected negative: {name} must return {expected}; its failure output is retained.", flush=True)
        try:
            helper.run_command(arguments + extra, fixture, output / f"{name}.log", environment)
            raise AssertionError(f"{name} unexpectedly passed")
        except RuntimeError:
            result = json.loads((output / f"{name}.command.json").read_text())
            assert result["exit_code"] == expected, result
            checks.append({"scenario": name, "status": "expected refusal", "command": result})
    (output / "receipt.json").write_text(json.dumps({
        "version": version, "repository": str(repository), "checks": checks,
    }, indent=2))
    print(f"nextest executor E2E passed: {len(checks)} real execution/refusal scenarios; {output}")


if __name__ == "__main__":
    main()
