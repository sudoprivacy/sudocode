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
from unittest.mock import patch

import psutil


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def git_environment_acceptance(helper, output):
    """Keep real fixture Git writes out of the repository that invoked the hook."""
    names = subprocess.check_output(["git", "rev-parse", "--local-env-vars"],
                                    cwd=helper.ROOT, text=True).splitlines()
    clean = {key: value for key, value in os.environ.items() if key not in names
             and not key.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"))}
    caller, foreign = output / "git-caller", output / "git-foreign"
    caller.mkdir()

    def git(*args):
        return subprocess.check_output(["git", *args], cwd=caller, env=clean,
                                       stderr=subprocess.STDOUT, text=True).strip()

    git("init", "--initial-branch=main")
    git("config", "user.name", "Caller fixture")
    git("config", "user.email", "caller@example.invalid")
    (caller / "sentinel.txt").write_text("Preserve the hook caller.\n")
    git("add", ".")
    git("commit", "-m", "Preserve caller")
    before = {name: (caller / ".git" / name).read_bytes() for name in ("HEAD", "index", "config")}
    head = git("rev-parse", "HEAD")
    child = output / "git-child.py"
    child.write_text('''import json,os,shutil,subprocess,sys
from pathlib import Path
mode,root,names=sys.argv[1],Path(sys.argv[2]),json.loads(sys.argv[3])
assert os.environ["GIT_TERMINAL_PROMPT"] == "0"
if mode != "explicit":
    assert not any(key in os.environ for key in names), "Hook repository selectors leaked into the child"
    assert not any(key.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_")) for key in os.environ)
if mode == "no-git":
    assert shutil.which("git") is None
elif mode == "fresh":
    root.mkdir()
    for args in [["init","--initial-branch=main"],["config","user.name","Foreign fixture"],
                 ["config","user.email","foreign@example.invalid"]]:
        subprocess.run(["git",*args],cwd=root,check=True)
    (root/"foreign.txt").write_text("Only the fixture repository receives this commit.\\n")
    subprocess.run(["git","add","."],cwd=root,check=True)
    subprocess.run(["git","commit","-m","Foreign commit"],cwd=root,check=True)
else:
    actual=subprocess.check_output(["git","rev-parse","--show-toplevel"],cwd=root,text=True).strip()
    assert Path(actual).resolve() == root.resolve()
    assert subprocess.check_output(["git","config","user.name"],cwd=root,text=True).strip() == "Explicit child"
''')
    inherited = dict.fromkeys(names, "Inherited hook selector")
    inherited.update({"GIT_DIR": str(caller / ".git"), "GIT_COMMON_DIR": str(caller / ".git"),
                 "GIT_WORK_TREE": str(caller), "GIT_INDEX_FILE": str(caller / ".git/index"),
                 "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "user.name",
                 "GIT_CONFIG_VALUE_0": "Inherited caller", "GIT_TERMINAL_PROMPT": "0"})
    with patch.dict(os.environ, inherited):
        fresh = helper.run_command([sys.executable, str(child), "fresh", str(foreign), json.dumps(names)],
                                   caller, output / "git-isolation.log")
        explicit = helper.run_command(
            [sys.executable, str(child), "explicit", str(foreign), json.dumps(names)],
            foreign, output / "git-explicit.log",
            {"GIT_DIR": str(foreign / ".git"), "GIT_CONFIG_COUNT": "1",
             "GIT_CONFIG_KEY_0": "user.name", "GIT_CONFIG_VALUE_0": "Explicit child"})
        without_git = helper.run_command(
            [sys.executable, str(child), "no-git", str(foreign), json.dumps(names)],
            caller, output / "git-unavailable.log", {"PATH": str(output / "empty-path")})
    assert all((caller / ".git" / name).read_bytes() == data for name, data in before.items())
    assert git("rev-parse", "HEAD") == head
    assert not git("status", "--porcelain")
    print("PASS: Git fixture preserves caller HEAD/index/config/source, honors explicit settings and works without Git on PATH")
    return {"scenario": "hook Git environment isolation", "status": "pass",
            "commands": [fresh, explicit, without_git]}


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
    git_environment = git_environment_acceptance(helper, output)
    checks = []

    def execute(command, log):
        # Real Cargo/native processes run while the unsafe name-to-argv fallback
        # is forbidden. This is an enforcement fault, not product/provider coverage.
        with patch.object(psutil.Process, "name", side_effect=AssertionError("The compiler monitor must not read argv")):
            return helper.run_command(command, fixture, log, environment)

    result = execute(arguments, output / "native-process.log")
    assert "1 passed, 1 skipped" in (output / "native-process.log").read_text(encoding="utf-8", errors="replace")
    checks.append({"scenario": "native child and ignored guard", "status": "pass", "command": result})
    source.write_text("fn main() { std::process::exit(42); }\n")
    for name, extra, expected in [("native-failure", [], 100),
                                   ("no-matching-cases", ["-E", "test(=missing_case)"], 4)]:
        print(f"Expected negative: {name} must return {expected}; its failure output is retained.", flush=True)
        try:
            execute(arguments + extra, output / f"{name}.log")
            raise AssertionError(f"{name} unexpectedly passed")
        except RuntimeError:
            result = json.loads((output / f"{name}.command.json").read_text())
            assert result["exit_code"] == expected, result
            checks.append({"scenario": name, "status": "expected refusal", "command": result})
    (output / "receipt.json").write_text(json.dumps({
        "version": version, "repository": str(repository), "checks": checks,
        "git_environment": git_environment,
        "unsafe_process_name_fallback_forbidden_during_real_commands": True,
    }, indent=2))
    print(f"nextest executor E2E passed: {len(checks)} real execution/refusal scenarios; {output}")


if __name__ == "__main__":
    main()
