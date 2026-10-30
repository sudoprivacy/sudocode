#!/usr/bin/env python3
"""Exercise the pinned runner with real Cargo builds and native child processes.

These are execution/enforcement checks, not product or live-model coverage.
The intentionally failing child and empty selection must never return success.
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def macos_observation_acceptance(helper, fixture, binary, output):
    if sys.platform != "darwin":
        return None
    # Exercise the real psutil C extension and owned native child. Inject the
    # kernel's errno=0 refusal, not a Python replacement for process inspection.
    source = output / "procargs-denied.c"
    source.write_text('''#include <errno.h>
#include <stddef.h>
#include <sys/sysctl.h>
static int denied_sysctl(int *name, u_int len, void *oldp, size_t *oldlenp,
                         void *newp, size_t newlen) {
    if (len >= 2 && name[0] == CTL_KERN && name[1] == KERN_PROCARGS2) {
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
    driver.write_text('''import importlib.util, json, subprocess, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location("executor", sys.argv[1])
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)
root, binary, output = map(Path, sys.argv[2:])
code = "import subprocess,sys; subprocess.run([sys.argv[1], 'observe'], check=True)"
result = helper.run_command([sys.executable, "-c", code, str(binary)], root, output / "observation.log")
assert result["exit_code"] == 0 and result["process_observation_denials"] > 0, result
assert "native-child-receipt" in (output / "observation.log").read_text()
(output / "observation-receipt.json").write_text(json.dumps(result, indent=2))
''')
    subprocess.run([sys.executable, str(driver), str(Path(helper.__file__).resolve()),
                    str(fixture), str(binary), str(output)], check=True,
                   env={**os.environ, "DYLD_INSERT_LIBRARIES": str(library)})
    result = json.loads((output / "observation-receipt.json").read_text())
    print("PASS: actual macOS procargs permission failures do not cancel the native child", flush=True)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    output = (args.output or (Path.home() / ".nexus/local-acceptance/nextest" / str(time.time_ns()))).resolve()
    if output == SOURCE or SOURCE in output.parents:
        raise RuntimeError("Keep nextest acceptance outputs outside the source checkout")
    output.mkdir(parents=True, exist_ok=False)
    profiles = load("acceptance_profiles", SOURCE / ".github/local-acceptance/checks.py")
    helper = load("acceptance_executor", SOURCE / ".github/scripts/local_acceptance.py")
    executable = profiles.nextest_binary(output)
    version = subprocess.check_output([str(executable), "nextest", "--version"], text=True).strip()
    if not version.startswith("cargo-nextest 0.9.150 "):
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
    arguments = [str(executable), "nextest", "run", *profiles.nextest_options(output),
                 "--manifest-path", str(fixture / "Cargo.toml"), "--locked"]
    environment = {"CARGO_TARGET_DIR": str(output / "target")}
    checks = []
    result = helper.run_command(arguments, fixture, output / "native-process.log", environment)
    log = (output / "native-process.log").read_text(encoding="utf-8", errors="replace")
    log = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", log)
    assert re.search(r"Summary\s+\[[^\]]+\]\s+1 test run: 1 passed(?: \([^)]*\))?, 1 skipped", log), log
    checks.append({"scenario": "native child and ignored guard", "status": "pass", "command": result})
    binary = output / "target/debug/nextest-executor-fixture"
    observation = macos_observation_acceptance(helper, fixture, binary, output)
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
        "version": version, "checks": checks, "macos_process_observation": observation,
    }, indent=2))
    print(f"nextest executor E2E passed: {len(checks)} real execution/refusal scenarios; {output}")


if __name__ == "__main__":
    main()
