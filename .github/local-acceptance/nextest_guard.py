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
import re
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


def macos_observation_acceptance(helper, fixture, binary, output):
    if sys.platform != "darwin":
        return None
    # Exercise the actual psutil C error path; the compiler observer must use
    # executable paths without invoking this denied argv syscall again.
    source = output / "procargs-denied.c"
    source.write_text('''#include <errno.h>
#include <stddef.h>
#include <sys/sysctl.h>
static unsigned int denied_calls = 0;
unsigned int scode_denied_argv_calls(void) { return denied_calls; }
static int denied_sysctl(int *name, u_int len, void *oldp, size_t *oldlenp,
                         void *newp, size_t newlen) {
    if (len >= 2 && name[0] == CTL_KERN && name[1] == KERN_PROCARGS2) {
        ++denied_calls;
        errno = 0;
        return -1;
    }
    return sysctl(name, len, oldp, oldlenp, newp, newlen);
}
__attribute__((used)) static struct { const void *replacement; const void *original; }
interpose __attribute__((section("__DATA,__interpose"))) = {
    (const void *)denied_sysctl, (const void *)sysctl
};
''')
    library = output / "procargs-denied.dylib"
    subprocess.run(["clang", "-dynamiclib", str(source), "-o", str(library)], check=True)
    driver = output / "observe-native.py"
    driver.write_text('''import ctypes, importlib.util, json, os, sys
from pathlib import Path
import psutil
spec = importlib.util.spec_from_file_location("executor", sys.argv[1])
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)
root, binary, output, library = map(Path, sys.argv[2:])
counter = ctypes.CDLL(str(library)).scode_denied_argv_calls
counter.restype = ctypes.c_uint
initial = counter()
try:
    psutil.Process(os.getpid()).cmdline()
except psutil.AccessDenied as error:
    fault_type = type(error).__name__
except SystemError as error:
    assert isinstance(error.__cause__ or error.__context__, PermissionError), error
    fault_type = type(error).__name__
else:
    raise AssertionError("The actual macOS argv fault was not injected")
after_probe = counter()
assert after_probe > initial
code = "import subprocess,sys; subprocess.run([sys.argv[1], 'observe'], check=True)"
result = helper.run_command([sys.executable, "-c", code, str(binary)], root, output / "observation.log")
assert result["exit_code"] == 0 and counter() == after_probe, (result, after_probe, counter())
assert "native-child-receipt" in (output / "observation.log").read_text()
proof = {"command": result, "argv_fault": {"type": fault_type, "before": initial,
         "after_probe": after_probe, "after_native_command": counter()}}
(output / "observation-receipt.json").write_text(json.dumps(proof, indent=2))
''')
    subprocess.run([sys.executable, str(driver), str(Path(helper.__file__).resolve()),
                    str(fixture), str(binary), str(output), str(library)], check=True,
                   env={**os.environ, "DYLD_INSERT_LIBRARIES": str(library)})
    result = json.loads((output / "observation-receipt.json").read_text())
    print("PASS: actual macOS argv fault is active; native commands complete without reading argv", flush=True)
    return result


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
    source.write_text('fn main() { if std::env::args().any(|arg| arg == "observe") { '
                      'std::thread::sleep(std::time::Duration::from_secs(2)); } '
                      'println!("native-child-receipt"); }\n')
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

    def execute(command, log):
        # Real Cargo/native processes run while the unsafe name-to-argv fallback
        # is forbidden. This is an enforcement fault, not product/provider coverage.
        with patch.object(psutil.Process, "name", side_effect=AssertionError("The compiler monitor must not read argv")):
            return helper.run_command(command, fixture, log, environment)

    result = execute(arguments, output / "native-process.log")
    log = (output / "native-process.log").read_text(encoding="utf-8", errors="replace")
    log = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", log)
    assert re.search(r"Summary\s+\[[^\]]+\]\s+1 test run: 1 passed(?: \([^)]*\))?, 1 skipped", log), log
    checks.append({"scenario": "native child and ignored guard", "status": "pass", "command": result})
    observation = macos_observation_acceptance(helper, fixture,
        output / "target/debug/nextest-executor-fixture", output)
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
        "unsafe_process_name_fallback_forbidden_during_real_commands": True,
        "macos_process_observation": observation,
    }, indent=2))
    print(f"nextest executor E2E passed: {len(checks)} real execution/refusal scenarios; {output}")


if __name__ == "__main__":
    main()
