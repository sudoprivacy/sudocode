#!/usr/bin/env python3
"""Measure release CLI process startup and enforce a median latency budget."""

import argparse
import json
import os
from pathlib import Path
import statistics
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", required=True, type=Path)
    parser.add_argument("--samples", type=int, default=40)
    parser.add_argument("--budget-ms", type=float, default=100)
    args = parser.parse_args()
    binary = str(args.bin.resolve())
    assert args.samples >= 5 and args.budget_ms > 0
    with tempfile.TemporaryDirectory(prefix="scode-startup-") as directory:
        root = Path(directory)
        config = root / "config"
        config.mkdir()
        env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("SCODE_", "SUDO_CODE_", "SUDOCODE_"))
        }
        env.update(HOME=directory, USERPROFILE=directory, SUDO_CODE_CONFIG_HOME=str(config), NO_COLOR="1")

        def invoke(command, trace=False):
            started = time.perf_counter()
            result = subprocess.run(
                [binary, *command], cwd=root,
                env={**env, **({"SCODE_TRACE_STARTUP": "1"} if trace else {})},
                capture_output=True, text=True, timeout=10, check=True,
            )
            elapsed = (time.perf_counter() - started) * 1000
            assert "startup phase=" not in result.stdout
            if trace:
                assert "startup phase=console " in result.stderr
                assert "startup phase=clap " in result.stderr
            else:
                assert not result.stderr, result.stderr
            return elapsed, result

        for command in (["--version"], ["version", "--output-format", "json"]):
            for _ in range(3):
                invoke(command)
            measurements = [invoke(command)[0] for _ in range(args.samples)]
            median = statistics.median(measurements)
            print(json.dumps({
                "command": command, "samples": args.samples,
                "median_ms": round(median, 3),
                "p95_ms": round(sorted(measurements)[int(args.samples * 0.95) - 1], 3),
                "budget_ms": args.budget_ms,
            }), flush=True)
            _, traced = invoke(command, trace=True)
            print(traced.stderr, end="", flush=True)
            if command[0] == "version":
                json.loads(traced.stdout)
                assert "startup phase=version_output " in traced.stderr
            assert median <= args.budget_ms, f"startup median {median:.3f} ms exceeds budget"


if __name__ == "__main__":
    main()
