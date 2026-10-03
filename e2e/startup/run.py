#!/usr/bin/env python3
"""Measure release CLI process startup and enforce a median latency budget."""

import argparse
import json
import math
import os
from pathlib import Path
import re
import statistics
import subprocess
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", required=True, type=Path)
    parser.add_argument("--samples", type=int, default=40)
    parser.add_argument("--budget-ms", type=float, default=100)
    parser.add_argument("--report", type=Path, help="save samples and phase timings as JSON")
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

        # Build outside the timed region. Same Rust toolchain, cwd, environment,
        # arguments, stdout bytes and pipe capture as the real CLI; no shell.
        reference = root / ("reference.exe" if os.name == "nt" else "reference")
        subprocess.run([
            "rustc", "--edition=2021", "-O", str(Path(__file__).with_name("reference.rs")),
            "-o", str(reference),
        ], check=True, timeout=120)
        report = {
            "binary": binary,
            "binary_bytes": Path(binary).stat().st_size,
            "reference_bytes": reference.stat().st_size,
            "commands": [],
        }

        def invoke(command, trace=False, executable=binary):
            started = time.perf_counter()
            result = subprocess.run(
                [str(executable), *command], cwd=root,
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
            _, expected = invoke(command)
            env["SCODE_STARTUP_REFERENCE_STDOUT"] = expected.stdout
            for _ in range(3):
                invoke(command)
                invoke(command, executable=reference)
            measurements, references = [], []
            for index in range(args.samples):
                # Alternate the order so gradual load changes do not always
                # favor the first executable. Keep each pair adjacent.
                order = [(binary, measurements), (reference, references)]
                if index % 2:
                    order.reverse()
                for executable, samples in order:
                    elapsed, result = invoke(command, executable=executable)
                    assert result.stdout == expected.stdout
                    samples.append(elapsed)
            median = statistics.median(measurements)
            summary = {
                "command": command, "samples": args.samples,
                "median_ms": round(median, 3),
                "p95_ms": round(sorted(measurements)[math.ceil(args.samples * 0.95) - 1], 3),
                "reference_median_ms": round(statistics.median(references), 3),
                "paired_difference_median_ms": round(statistics.median([
                    actual - reference for actual, reference in zip(measurements, references)
                ]), 3),
                "budget_ms": args.budget_ms,
            }
            traced_wall, traced = invoke(command, trace=True)
            phases = [{"phase": name, "elapsed_us": int(elapsed), "cumulative_us": int(cumulative)}
                      for name, elapsed, cumulative in re.findall(
                          r"startup phase=(\w+) elapsed_us=(\d+) cumulative_us=(\d+)", traced.stderr)]
            assert phases, traced.stderr
            summary["traced_wall_ms"] = round(traced_wall, 3)
            summary["traced_main_ms"] = round(max(p["cumulative_us"] for p in phases) / 1000, 3)
            print(json.dumps(summary), flush=True)
            print(traced.stderr, end="", flush=True)
            report["commands"].append({
                **summary, "samples_ms": measurements, "reference_samples_ms": references,
                "phases": phases,
            })
            if args.report:
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
            if command[0] == "version":
                json.loads(traced.stdout)
                assert "startup phase=version_output " in traced.stderr
            assert median <= args.budget_ms, f"startup median {median:.3f} ms exceeds budget"


if __name__ == "__main__":
    main()
