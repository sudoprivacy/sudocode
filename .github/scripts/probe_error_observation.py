"""Bounded native diagnostic; neither a CI retry nor candidate acceptance."""
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
CLI_SHA256 = "c2ed7ad1a1ad361a787286e594dcc2d34a378b0628bb18cc9a29d2938c57fee4"
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
    "available_cpus": os.cpu_count(),
    "predecessor_artifact_run": 40791713774,
    "conditions": [],
}


def save():
    (OUT / "receipt.json").write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")


save()
build = OUT / "compile.jsonl"
names = ["pty_bracketed_paste", "pty_cache_prefix", "pty_compaction_safety"]
command = ["cargo", "test", "--workspace", "--no-run", "--message-format=json"]
for name in names:
    command.extend(["--test", name])
with build.open("wb") as stream:
    built = subprocess.run(command, cwd=ROOT / "rust", stdout=stream, stderr=subprocess.STDOUT)
assert built.returncode == 0
executables = {}
for line in build.read_text(encoding="utf-8", errors="replace").splitlines():
    try:
        item = json.loads(line)
    except ValueError:
        continue
    name = item.get("target", {}).get("name")
    if item.get("reason") == "compiler-artifact" and name in names and item.get("executable"):
        executables[name] = Path(item["executable"])
assert set(executables) == set(names), executables
images = OUT / "bin"
images.mkdir()
sealed_cli = images / "scode-before-observation-fix.exe"
previous_cli = ROOT / "previous-probe/bin/scode-before-observation-fix.exe"
assert hashlib.sha256(previous_cli.read_bytes()).hexdigest() == CLI_SHA256
shutil.copy2(previous_cli, sealed_cli)
sealed = {}
for name, path in executables.items():
    sealed[name] = images / f"{name}-diagnostic.exe"
    shutil.copy2(path, sealed[name])
record["images"] = {
    str(path.relative_to(OUT)): hashlib.sha256(path.read_bytes()).hexdigest()
    for path in [sealed_cli, *sealed.values()]
}
target_libdir = subprocess.check_output(
    ["rustc", "--print", "target-libdir"], text=True).strip()
record["cargo_runtime_paths"] = [str(next(iter(executables.values())).parent),
                                 str(ROOT / "rust/target/debug"), target_libdir]
save()


def condition(name, suite, threads=None):
    env = dict(os.environ, SCODE_TEST_BACKEND="mock", SCODE_TEST_BIN=str(sealed_cli),
               SCODE_ERROR_OBSERVATION_PROBE_MODE="raw")
    for key in ["RUST_TEST_THREADS", "SCODE_ERROR_OBSERVATION_PROBE_COLUMNS"]:
        env.pop(key, None)
    env["PATH"] = os.pathsep.join(record["cargo_runtime_paths"] + [env.get("PATH", "")])
    command = [str(sealed[suite]), "--nocapture"]
    if threads is not None:
        command.append(f"--test-threads={threads}")
    log = OUT / f"{name}.log"
    started = time.monotonic()
    with log.open("xb") as stream:
        result = subprocess.run(command, cwd=ROOT / "rust", env=env,
                                stdout=stream, stderr=subprocess.STDOUT)
    observations, phases = [], []
    for line in log.read_text(encoding="utf-8", errors="replace").splitlines():
        for marker, rows in [("ERROR_PROBE_OBSERVATION ", observations),
                             ("ERROR_PROBE_PHASE ", phases)]:
            if marker in line:
                rows.append(json.loads(line.split(marker, 1)[1]))
    row = {"name": name, "suite": suite, "geometry": "original default; no resize",
           "threads": threads or "original default", "exit_code": result.returncode,
           "seconds": round(time.monotonic() - started, 2), "command": command,
           "log": str(log.relative_to(OUT)), "observations": observations, "phases": phases}
    record["conditions"].append(row)
    save()
    print(json.dumps({key: row[key] for key in ["name", "exit_code", "seconds"]}), flush=True)
    return row


# Preserve the original prelude, geometry, suite breadth and default concurrency.
prelude = condition("original-bracketed-paste-prelude", "pty_bracketed_paste")
if prelude["exit_code"] != 0:
    record["unclassified_prelude_failure"] = True
    save()
    sys.exit(2)
original = condition("original-full-cache-suite", "pty_cache_prefix")
if original["exit_code"] == 0:
    scoped = condition("original-full-compaction-suite", "pty_compaction_safety")
    if scoped["exit_code"] != 0:
        record["unclassified_compaction_failure"] = True
        save()
        sys.exit(2)
    # A single declared pressure condition, never a run-until-green loop.
    last = condition("full-cache-suite-pressure-16", "pty_cache_prefix", threads=16)
else:
    last = original
record["negative_observed"] = last["exit_code"] != 0
record["raw_render_mismatch_proved"] = any(
    item["raw_timeout"] and item["visible"] and item["raw_found_after_failure"] is False
    and item["requests"] == item["expected_requests"]
    and item["archives"] == item["expected_archives"]
    for item in last["observations"])
record["not_reproduced"] = not record["negative_observed"]
record["images_unchanged"] = all(
    hashlib.sha256((OUT / path).read_bytes()).hexdigest() == digest
    for path, digest in record["images"].items())
save()
# No green acceptance: either a preserved failure, or an inconclusive probe.
sys.exit(2 if record["negative_observed"] else 3)
