#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from collections import defaultdict
from pathlib import Path


def load_manifest(path: Path) -> list[dict]:
    return json.loads(path.read_text())


def load_parity_text(path: Path) -> str:
    return path.read_text()


def ensure_refs_exist(manifest: list[dict], parity_text: str) -> list[tuple[str, str]]:
    missing: list[tuple[str, str]] = []
    for entry in manifest:
        for ref in entry.get("parity_refs", []):
            if ref not in parity_text:
                missing.append((entry["name"], ref))
    return missing


def run_harness(rust_root: Path) -> dict:
    with tempfile.TemporaryDirectory(prefix="mock-parity-report-") as temp_dir:
        report_path = Path(temp_dir) / "report.json"
        env = os.environ.copy()
        env["MOCK_PARITY_REPORT_PATH"] = str(report_path)
        subprocess.run(
            [
                "cargo",
                "test",
                "-p",
                "rusty-sudocode-cli",
                "--test",
                "mock_parity_harness",
                "--",
                "--nocapture",
            ],
            cwd=rust_root,
            check=True,
            env=env,
        )
        return json.loads(report_path.read_text())


def main() -> int:
    script_path = Path(__file__).resolve()
    rust_root = script_path.parent.parent
    repo_root = rust_root.parent
    manifest = load_manifest(rust_root / "mock_parity_scenarios.json")
    parity_text = load_parity_text(repo_root / "PARITY.md")

    missing_refs = ensure_refs_exist(manifest, parity_text)
    if missing_refs:
        print("Missing PARITY.md references:", file=sys.stderr)
        for scenario_name, ref in missing_refs:
            print(f"  - {scenario_name}: {ref}", file=sys.stderr)
        return 1

    should_run = "--no-run" not in sys.argv[1:]
    report = run_harness(rust_root) if should_run else None
    report_by_name = {
        entry["name"]: entry for entry in report.get("scenarios", [])
    } if report else {}

    print("Mock parity diff checklist")
    print(f"Repo root: {repo_root}")
    print(f"Scenario manifest: {rust_root / 'mock_parity_scenarios.json'}")
    print(f"PARITY source: {repo_root / 'PARITY.md'}")
    print()

    for entry in manifest:
        scenario_name = entry["name"]
        scenario_report = report_by_name.get(scenario_name)
        status = "PASS" if scenario_report else ("MAPPED" if not should_run else "MISSING")
        print(f"[{status}] {scenario_name} ({entry['category']})")
        print(f"  description: {entry['description']}")
        print(f"  parity refs: {' | '.join(entry['parity_refs'])}")
        if scenario_report:
            print(
                "  result: iterations={iterations} requests={requests} tool_uses={tool_uses} tool_errors={tool_errors}".format(
                    iterations=scenario_report["iterations"],
                    requests=scenario_report["request_count"],
                    tool_uses=", ".join(scenario_report["tool_uses"]) or "none",
                    tool_errors=scenario_report["tool_error_count"],
                )
            )
            print(f"  final: {scenario_report['final_message']}")
        print()

    coverage = defaultdict(list)
    for entry in manifest:
        for ref in entry["parity_refs"]:
            coverage[ref].append(entry["name"])

    print("PARITY coverage map")
    for ref, scenarios in coverage.items():
        print(f"- {ref}")
        print(f"  scenarios: {', '.join(scenarios)}")

    if report and report.get("scenarios"):
        first = report["scenarios"][0]
        print()
        print("First scenario result")
        print(f"- name: {first['name']}")
        print(f"- iterations: {first['iterations']}")
        print(f"- requests: {first['request_count']}")
        print(f"- tool_uses: {', '.join(first['tool_uses']) or 'none'}")
        print(f"- tool_errors: {first['tool_error_count']}")
        print(f"- final_message: {first['final_message']}")
        print()
        print(
            "Harness summary: {scenario_count} scenarios, {request_count} requests".format(
                scenario_count=report["scenario_count"],
                request_count=report["request_count"],
            )
        )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
