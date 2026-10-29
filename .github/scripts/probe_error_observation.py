"""Causal native-terminal probe; this is not candidate PR acceptance."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

ROOT = Path.cwd()
OUT = ROOT / "error-observation-probe"
OUT.mkdir(exist_ok=False)
BASE = "63f569ce885c6bce0939b8f956e4683f2690f242"
CASE = "thinking_repair_is_bounded_and_unrelated_bad_requests_preserve_history"
allowed = {
    ".github/workflows/rust-ci.yml",
    ".github/scripts/probe_error_observation.py",
    "rust/crates/rusty-sudocode-cli/tests/pty_cache_prefix.rs",
}
changed = set(subprocess.check_output(
    ["git", "diff", "--name-only", BASE, "HEAD"], text=True).splitlines())
assert changed <= allowed, changed
record = {
    "base": BASE,
    "head": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
    "scope": "Independent diagnostic; not a retry or acceptance of either PR head",
    "production_source_unchanged": True,
    "changed_paths": sorted(changed),
    "toolchain": subprocess.check_output(["rustc", "--version"], text=True).strip(),
    "controls": [],
}


def save():
    (OUT / "receipt.json").write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")


save()
build = OUT / "compile.jsonl"
with build.open("wb") as stream:
    built = subprocess.run([
        "cargo", "test", "--workspace", "--test", "pty_cache_prefix", "--no-run",
        "--message-format=json",
    ], cwd=ROOT / "rust", stdout=stream, stderr=subprocess.STDOUT)
assert built.returncode == 0
messages = []
for line in build.read_text(encoding="utf-8", errors="replace").splitlines():
    try:
        messages.append(json.loads(line))
    except ValueError:
        pass
test = next(Path(item["executable"]) for item in messages
            if item.get("reason") == "compiler-artifact"
            and item.get("target", {}).get("name") == "pty_cache_prefix"
            and item.get("executable"))
cli = ROOT / "rust/target/debug/scode.exe"
images = OUT / "bin"
images.mkdir()
sealed_cli = images / "scode-before-observation-fix.exe"
sealed_test = images / "pty-cache-prefix-observation-probe.exe"
for source, sealed in [(cli, sealed_cli), (test, sealed_test)]:
    assert source.is_file(), source
    shutil.copy2(source, sealed)
record["images"] = {
    str(path.relative_to(OUT)): hashlib.sha256(path.read_bytes()).hexdigest()
    for path in [sealed_cli, sealed_test]
}
target_libdir = subprocess.check_output(
    ["rustc", "--print", "target-libdir"], text=True).strip()
record["cargo_runtime_paths"] = [str(test.parent), str(cli.parent), target_libdir]
save()


def control(columns, mode):
    env = dict(os.environ, SCODE_TEST_BACKEND="mock", SCODE_TEST_BIN=str(sealed_cli),
               SCODE_ERROR_OBSERVATION_PROBE_COLUMNS=str(columns),
               SCODE_ERROR_OBSERVATION_PROBE_MODE=mode)
    env["PATH"] = os.pathsep.join(record["cargo_runtime_paths"] + [env.get("PATH", "")])
    command = [str(sealed_test), "--exact", CASE, "--test-threads=1", "--nocapture"]
    log = OUT / f"{columns}-{mode}.log"
    started = time.monotonic()
    with log.open("xb") as stream:
        result = subprocess.run(command, cwd=ROOT / "rust", env=env,
                                stdout=stream, stderr=subprocess.STDOUT)
    observations = []
    for line in log.read_text(encoding="utf-8", errors="replace").splitlines():
        if "ERROR_PROBE_OBSERVATION " in line:
            observations.append(json.loads(line.split("ERROR_PROBE_OBSERVATION ", 1)[1]))
    row = {"columns": columns, "mode": mode, "exit_code": result.returncode,
           "seconds": round(time.monotonic() - started, 2), "command": command,
           "log": str(log.relative_to(OUT)), "observations": observations}
    record["controls"].append(row)
    save()
    print(json.dumps({key: row[key] for key in ["columns", "mode", "exit_code", "seconds"]}),
          flush=True)
    return row


for columns in [64, 72, 80, 88, 96, 120]:
    before = control(columns, "raw")
    if before["exit_code"] == 0:
        continue
    valid = any(item["raw_failed"] and item["visible"]
                and item["requests"] == item["expected_requests"]
                and item["archives"] == item["expected_archives"]
                for item in before["observations"])
    record["negative_control_valid"] = valid
    if not valid:
        record["unclassified_failure"] = True
        save()
        sys.exit(2)
    after = control(columns, "screen")
    record["positive_control_passed"] = after["exit_code"] == 0
    record["images_unchanged"] = all(
        hashlib.sha256((OUT / path).read_bytes()).hexdigest() == digest
        for path, digest in record["images"].items())
    save()
    sys.exit(0 if record["positive_control_passed"] and record["images_unchanged"] else 1)
record["negative_control_valid"] = False
record["not_reproduced"] = True
save()
sys.exit(3)
