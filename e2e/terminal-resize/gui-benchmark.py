"""Painted-terminal comparison using the same paired evaluator as the core gate."""

import argparse
import json
import math
import os
from pathlib import Path
import subprocess
import sys

from benchmark import assess, comparison_rounds, digest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["driver", "base", "candidate", "output"]:
        parser.add_argument("--" + name, required=True, type=Path)
    args = parser.parse_args()
    if args.output.exists() and any(args.output.iterdir()):
        parser.error("choose an empty output directory to preserve previous evidence")
    args.output.mkdir(parents=True, exist_ok=True)
    policy = json.loads(Path(__file__).with_name("gui-policy.json").read_text())
    rounds = comparison_rounds(policy, "browser")
    print(f"Browser paired matrix: {rounds['calibration']} A/A pairs, {rounds['comparison']} A/B pairs, "
          "plus two warm-ups. Per-run interactions and budgets are unchanged.", flush=True)
    sampling = policy["sampling"]
    keys = len(sampling["input_delays_ms"])
    sample_counts = {"drag_frame_ms": sampling["drag_steps"]}
    for phase in ["history", "wait", "stream"]:
        sample_counts[f"input_{phase}_parsed_ms"] = keys * sampling["parsed_bursts"]
        sample_counts[f"input_{phase}_frame_ms"] = keys * sampling["painted_bursts"]
    assert keys >= 20 and sample_counts["input_wait_parsed_ms"] >= 100
    report = {"version": 1, "status": "running", "policy": policy, "samples": [],
              "base_sha256": digest(args.base), "candidate_sha256": digest(args.candidate),
              "driver_sha256": digest(args.driver)}
    report["harness_sha256"] = {name: digest(Path(__file__).with_name(name)) for name in
                                ["gui-benchmark.py", "gui.cjs", "gui.html", "host.cjs", "resources.cjs", "benchmark.py"]}
    controls, comparisons = {"browser": []}, {"browser": []}
    browser_metadata = None

    def save():
        (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")

    def sample(binary, label, expect_links):
        nonlocal browser_metadata
        folder = args.output / label
        folder.mkdir()
        output = folder / "sample.json"
        env = dict(os.environ, SCODE_TEST_BIN=str(binary.resolve()), SCODE_TEST_BACKEND="mock",
                   SCODE_GUI_REPORT=str(output.resolve()), SCODE_TERMINAL_LOG_DIR=str(folder.resolve()),
                   SCODE_GUI_EXPECT_LINKS="1" if expect_links else "0")
        print(label, flush=True)
        with (folder / "driver.log").open("w") as log:
            result = subprocess.run([str(args.driver.resolve()), "--ignored", "--exact",
                                     "browser_terminal_links_and_interaction", "--nocapture"],
                                    env=env, stdout=log, stderr=subprocess.STDOUT, timeout=180)
        assert result.returncode == 0, f"failed {label}: {folder / 'driver.log'}"
        data = json.loads(output.read_text())
        assert data["status"] == "passed" and data["idle_bytes"] == 0, "invalid browser run"
        assert data["streaming_selection_preserved"], "missing streaming selection acceptance"
        metadata = (data["browser"], data["xterm"], data["environment"])
        if browser_metadata is None:
            browser_metadata = metadata
        assert metadata == browser_metadata, "browser/observer changed during comparison"
        assert all(isinstance(value, (int, float)) and math.isfinite(value) and value >= 0
                   for value in data["metrics"].values()), "invalid metric"
        assert set(data["metrics"]) == set(policy["metrics"]), "missing/unbudgeted browser metrics"
        assert set(data["samples"]) == set(sample_counts), "missing interaction workload"
        for name, values in data["samples"].items():
            assert all(math.isfinite(value) and value >= 0 for value in values)
            assert len(values) == sample_counts[name], "missing interaction samples"
        report["samples"].append({"label": label, "path": str(output.relative_to(args.output)), "data": data})
        save()
        return data

    try:
        sample(args.base, "warm-base", False)
        sample(args.candidate, "warm-candidate", True)
        for stage, count, target in [("aa", rounds["calibration"], controls), ("ab", rounds["comparison"], comparisons)]:
            for index in range(count):
                pair = {}
                for side in ["base", "candidate"] if index % 2 == 0 else ["candidate", "base"]:
                    candidate = stage == "ab" and side == "candidate"
                    pair[side] = sample(args.candidate if candidate else args.base,
                                        f"{stage}-{index:02}-{side}", candidate)
                target["browser"].append((pair["base"], pair["candidate"]))
            rows, failures = assess(controls, None if stage == "aa" else comparisons,
                                     policy, ["browser"], lambda _: policy["metrics"])
            report.update(metrics=rows, failures=failures)
            save()
            if failures:
                report["status"] = "noisy" if stage == "aa" else "failed"
                break
        else:
            assert digest(args.base) == report["base_sha256"], "baseline changed during measurement"
            assert digest(args.candidate) == report["candidate_sha256"], "candidate changed during measurement"
            assert digest(args.driver) == report["driver_sha256"], "driver changed during measurement"
            report["status"] = "passed"
    except (AssertionError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        report.update(status="invalid", failures=[str(error)])
    finally:
        save()
    lines = [f"Browser terminal: **{report['status']}**", "",
             "Frame/drag latency includes browser dispatch and PNG readback after rendered frames; not physical display latency.",
             "Parsed latency uses an independent burst without screenshots, from browser keydown to onWriteParsed containing the expected draft. Transport timestamps are retained separately.",
             "Browser RSS sums its dedicated process tree; shared pages may be counted more than once.", "",
             "| Metric | Baseline | Candidate | Result |", "| --- | ---: | ---: | --- |"]
    for row in report.get("metrics", []):
        lines.append(f"| {row['metric']} | {row.get('baseline', '—')} | {row.get('candidate', '—')} | {row['result']} |")
    lines.extend(["", *report.get("failures", [])])
    summary = "\n".join(lines) + "\n"
    (args.output / "summary.md").write_text(summary)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as stream:
            stream.write(summary)
    print(summary)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
