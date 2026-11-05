"""Private paired CI trial; no signing, acceptance status or policy change."""
import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import time


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def object_json(path, key):
    text = path.read_text(encoding="utf-8", errors="replace")
    decoder = json.JSONDecoder()
    for match in re.finditer(r"\{", text):
        try:
            value, _ = decoder.raw_decode(text[match.start():])
        except ValueError:
            continue
        if isinstance(value, dict) and key in value:
            return value
    raise RuntimeError(f"No actual {key} JSON in {path}")


def successful_summary(text, active, ignored):
    """Require complete success while allowing nextest's informational slow count."""
    return re.search(
        rf"Summary\s+\[\s*([\d.]+)s\]\s+{active} tests run: {active} passed"
        rf"(?: \(\d+ slow\))?, {ignored} skipped\s*$", text,
        flags=re.MULTILINE,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root, output = args.source.resolve(), args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    helper = load("probe_host", root / ".github/scripts/local_acceptance.py")
    helper.ROOT = root
    checks = load("probe_checks", root / ".github/local-acceptance/checks.py")
    layout = load("probe_cache", root / ".github/local-acceptance/cache_layout.py")
    target = Path(layout.resolve(Path(os.environ["GITHUB_WORKSPACE"]), root / "rust")["target"])
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    assert subprocess.check_output(["git", "status", "--porcelain"], cwd=root) == b""
    environment = {"SCODE_TEST_BACKEND": "mock", "CARGO_TARGET_DIR": str(target),
                   "CARGO_BUILD_JOBS": str((os.cpu_count() or 1) if os.environ.get("GITHUB_ACTIONS") == "true" else 1)}
    record = {"head": head, "started_at": datetime.now(timezone.utc).isoformat(),
              "host": platform.platform(), "logical_cpus": os.cpu_count(), "phases": [],
              "baseline_run": None,
              "baseline_job": None,
              "scope": "Full eight-worker native suite, captured immutable binaries and complete case inventory, original deadlines and no retries. Current protected main after198 Rust files changed since the prior field. No current same-source four-worker baseline; this is full native suite validation at eight workers, not a speedup measurement. No acceptance signature or paid API call."}
    started = time.monotonic()

    def invoke(command, log):
        return helper.run_command(command, root / "rust", output / log, environment)

    def hashes(paths):
        return {str(path): hashlib.file_digest(path.open("rb"), "sha256").hexdigest() for path in sorted(paths)}

    try:
        if os.name == "nt" and os.environ.get("GITHUB_ACTIONS") == "true":
            invoke(["netsh", "int", "ipv4", "set", "dynamicport", "tcp", "start=10000", "num=55535"],
                   "host-setup.log")
        tool = checks.nextest_binary(output)
        record["nextest"] = str(tool)
        invoke(["cargo", "metadata", "--format-version=1", "--locked", "--features", "engine-acp/mailbox"], "metadata.log")
        metadata = object_json(output / "metadata.log", "packages")
        (output / "cargo-metadata.json").write_text(json.dumps(metadata), encoding="utf-8")
        targets, selection = checks.grouped_workspace_test_targets(root, metadata)
        arguments = checks.workspace_integration_test_targets(targets)
        assert arguments and len(selection["groups"]) == 8
        record["target_selection"] = selection
        invoke([str(tool), "nextest", "list", "--list-type", "binaries-only", "--message-format", "json",
                "--workspace", "--locked", "--features", "engine-acp/mailbox", *arguments], "build.log")
        binaries = object_json(output / "build.log", "rust-binaries")
        (output / "binaries.json").write_text(json.dumps(binaries), encoding="utf-8")
        reused = ["--cargo-metadata", str(output / "cargo-metadata.json"),
                  "--binaries-metadata", str(output / "binaries.json")]
        config = output / "nextest.toml"
        config.write_text('[profile.default]\nretries = 0\nfail-fast = { max-fail = 1, terminate = "wait" }\n', encoding="utf-8")
        invoke([str(tool), "nextest", "list", "--message-format", "json", "--config-file", str(config),
                "--profile", "default", "--ignore-default-filter", *reused], "inventory.log")
        inventory = object_json(output / "inventory.log", "rust-suites")
        (output / "inventory.json").write_text(json.dumps(inventory), encoding="utf-8")
        expected = set()
        ignored = 0
        for identity, suite in inventory["rust-suites"].items():
            if suite["testcases"]:
                assert suite["kind"] == "test", suite
            for name, case in suite["testcases"].items():
                if case["ignored"]:
                    ignored += 1
                else:
                    expected.add((identity, name))
        # Current protected helpers select every integration target; the captured
        # pinned-runner inventory below defines the exact complete case set.
        assert len(expected) >= 1000 and ignored > 0, (len(expected), ignored)
        paths = set()

        def collect(value):
            if isinstance(value, dict):
                for key, item in value.items():
                    if key == "binary-path":
                        path = Path(item)
                        assert path.is_file(), path
                        paths.add(path)
                    collect(item)
            elif isinstance(value, list):
                for item in value:
                    collect(item)

        collect(binaries)
        paths.add(target / "debug" / ("scode.exe" if os.name == "nt" else "scode"))
        paths.update((target / "debug/deps").glob("*.dylib"))
        paths.update((target / "debug/deps").glob("*.dll"))
        assert len(paths) >= 80, len(paths)
        before = hashes(paths)
        record["binary_hashes"] = before
        record.update(expected_active_cases=len(expected), ignored_cases=ignored)
        (output / "configuration.json").write_text(json.dumps(record, indent=2), encoding="utf-8")
        actual_sets = []
        for workers in [8]:
            print(f"REAL_WORKER_FIELD workers={workers} cases={len(expected)} ignored={ignored}", flush=True)
            try:
                row = invoke([str(tool), "nextest", "run", "--config-file", str(config), "--profile", "default",
                              "--ignore-default-filter", "--test-threads", str(workers), "--retries", "0",
                              "--max-fail", "1:wait", "--no-tests", "fail", *reused], f"workers-{workers}.log")
            finally:
                record["binary_hashes_after"] = hashes(paths)
            phase = {"workers": workers, **row}
            record["phases"].append(phase)
            (output / "partial-result.json").write_text(json.dumps(record, indent=2), encoding="utf-8")
            text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", (output / f"workers-{workers}.log").read_text(encoding="utf-8", errors="replace"))
            cases = {(m[1], m[2]) for m in re.finditer(r"\bPASS\s+\[\s*[\d.]+s\]\s+\(\s*\d+/\d+\)\s+(\S+)\s+([^\r\n]+)", text)}
            assert cases == expected, {"missing": sorted(expected - cases), "extra": sorted(cases - expected)}
            summary = successful_summary(text, len(expected), ignored)
            assert summary, text[-1500:]
            assert record["binary_hashes_after"] == before, "A measured binary changed"
            actual_sets.append(cases)
            phase.update(cases=len(cases), test_seconds=float(summary[1]))
            (output / "partial-result.json").write_text(json.dumps(record, indent=2), encoding="utf-8")
            print(f"REAL_WORKER_RESULT workers={workers} seconds={row['elapsed_seconds']}", flush=True)
        assert actual_sets == [expected]
        assert subprocess.check_output(["git", "status", "--porcelain"], cwd=root) == b""
        record.update(status="PASS", full_inventory_matches_actual_cases=True,
                      captured_binaries_unchanged_during_measurement=True)
    except BaseException as error:
        record.update(status="FAIL", error=repr(error))
        raise
    finally:
        record.update(seconds=time.monotonic() - started, finished_at=datetime.now(timezone.utc).isoformat())
        (output / "result.json").write_text(json.dumps(record, indent=2), encoding="utf-8")
        print(json.dumps({key: value for key, value in record.items() if key not in {"binary_hashes", "binary_hashes_after", "target_selection"}}), flush=True)


if __name__ == "__main__":
    main()
