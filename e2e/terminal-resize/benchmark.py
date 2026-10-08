"""Paired release measurements using the existing Rust fixture and terminal host."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import random
import statistics
import subprocess
import sys


def quantile(values, fraction):
    return sorted(values)[max(0, math.ceil(len(values) * fraction) - 1)]


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def expected_metrics(spec):
    assert spec["kind"] in ["prose", "markdown", "background", "foreground"], "unsupported workload kind"
    metrics = {"idle_cpu_ms", "cpu_ms", "rss_peak_mib", "rss_after_mib",
               "rss_growth_mib", "wire_kib", "resize_ms", "paste_ms"}
    phases = ["stream", "history"]
    if spec["kind"] not in ["background", "foreground"]:
        phases.append("wait")
        metrics.update(["first_visible_ms", "cancel_ms"])
    metrics.update(f"input_{phase}_{percentile}_ms" for phase in phases
                   for percentile in ["p50", "p95"])
    return metrics


def validate(sample, case, policy):
    assert sample["version"] == 1 and sample["status"] == "complete", "incomplete workload"
    assert sample["case"] == case and sample["spec"] == policy["cases"][case], "changed workload"
    assert len(sample["turns"]) == sample["spec"]["turns"], "missing turns"
    assert sample["contracts"] == policy["contracts"], f"render contract failed: {sample['contracts']}"
    assert set(sample["metrics"]) == expected_metrics(sample["spec"]), "missing/extra measurements"
    for metric, value in sample["metrics"].items():
        assert isinstance(value, (int, float)) and math.isfinite(value) and value >= 0, metric
        assert metric in policy["metrics"], f"unbudgeted measurement {metric}"
    for phase in ["wait", "stream", "history"]:
        if f"input_{phase}_p95_ms" not in sample["metrics"]:
            continue
        values = sample["input_samples_ms"][phase]
        assert len(values) == policy["sampling"]["input"] * sample["spec"]["turns"], "missing input observations"
        assert all(math.isfinite(value) and value >= 0 for value in values), "invalid input observations"
    actions = sample["action_samples_ms"]
    assert len(actions["resize"]) == policy["sampling"]["resize"] * sample["spec"]["turns"]
    assert len(actions["paste"]) == sample["spec"]["turns"]
    assert len(actions["cancel"]) == (0 if sample["spec"]["kind"] in ["background", "foreground"]
                                      else policy["sampling"]["cancel"])
    assert all(math.isfinite(value) and value >= 0 for values in actions.values() for value in values)


def confidence_lower(differences):
    # Paired bootstrap; a fixed seed makes evaluation of a saved report repeatable.
    rng = random.Random(20261007)
    return quantile([statistics.median(rng.choices(differences, k=len(differences)))
                     for _ in range(4000)], 0.05)


def assess(controls, comparisons, policy, cases):
    rows, failures = [], []
    for case in cases:
        for metric in sorted(expected_metrics(policy["cases"][case])):
            budget = policy["metrics"][metric]
            calibration = [(left["metrics"][metric], right["metrics"][metric])
                           for left, right in controls[case]]
            deviations = [abs(right - left) for left, right in calibration]
            noise = statistics.median(deviations)
            noise_p90 = quantile(deviations, 0.9)
            control = statistics.median([value for pair in calibration for value in pair])
            noise_limit = max(budget["absolute"], budget["relative"] * control)
            row = {"case": case, "metric": metric, "noise": noise, "noise_p90": noise_p90,
                   "noise_limit": noise_limit}
            # Both calibration and comparison estimate a median across runs.
            # Keep isolated control outliers in the report; they must not turn
            # a stable median into a rejected measurement. Persistent variation
            # still fails calibration, and fixed budgets remain enforced.
            if noise > noise_limit:
                row["result"] = "noisy"
                failures.append(f"{case}/{metric}: A/A median/P90 noise {noise:.3f}/{noise_p90:.3f}; "
                                f"median limit {noise_limit:.3f}")
            elif comparisons is None:
                row["result"] = "calibrated"
            else:
                pairs = comparisons[case]
                baseline = statistics.median([left["metrics"][metric] for left, _ in pairs])
                candidate = statistics.median([right["metrics"][metric] for _, right in pairs])
                differences = [right["metrics"][metric] - left["metrics"][metric] for left, right in pairs]
                threshold = max(budget["absolute"], budget["relative"] * baseline)
                lower = confidence_lower(differences)
                row.update(baseline=baseline, candidate=candidate,
                           difference=statistics.median(differences), lower_95=lower,
                           threshold=threshold, limit=budget["limit"], result="pass")
                if candidate > budget["limit"]:
                    row["result"] = "over_budget"
                    failures.append(f"{case}/{metric}: {candidate:.3f} > fixed budget {budget['limit']}")
                elif lower > threshold:
                    row["result"] = "regressed"
                    failures.append(f"{case}/{metric}: paired increase {lower:.3f} > {threshold:.3f}")
            rows.append(row)
    return rows, failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver", type=Path, required=True, help="compiled pty_render_performance test")
    parser.add_argument("--base", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--policy", type=Path, default=Path(__file__).with_name("performance-policy.json"))
    parser.add_argument("--profile", choices=["core", "stress"], default="core")
    parser.add_argument("--preview", choices=["0", "1"], default="0")
    args = parser.parse_args()
    policy = json.loads(args.policy.read_text())
    assert policy["version"] == 1
    assert policy["rounds"]["calibration"] >= 5 and policy["rounds"]["comparison"] >= 7
    assert policy["sampling"]["input"] == 60 and policy["sampling"]["resize"] >= 40
    assert policy["sampling"]["resize"] % 2 == 0 and policy["sampling"]["cancel"] >= 20
    cases = policy["profiles"][args.profile]
    assert cases and len(cases) == len(set(cases)), "empty/duplicate matrix"
    for case in cases:
        assert expected_metrics(policy["cases"][case]) <= policy["metrics"].keys()
    if args.output.exists() and any(args.output.iterdir()):
        parser.error("output directory is not empty; choose a fresh path to preserve earlier evidence")
    args.output.mkdir(parents=True, exist_ok=True)
    report = {"version": 1, "profile": args.profile, "preview": args.preview,
              "policy_sha256": digest(args.policy), "policy": policy,
              "base_sha256": digest(args.base), "candidate_sha256": digest(args.candidate),
              "driver_sha256": digest(args.driver), "samples": [], "status": "running"}
    report["harness_sha256"] = {name: digest(Path(__file__).with_name(name))
                                for name in ["benchmark.py", "host.cjs", "run.cjs", "performance.cjs"]}
    controls, comparisons = {case: [] for case in cases}, {case: [] for case in cases}
    metadata = None

    def save():
        (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")

    def sample(binary, case, stage, round_number, side):
        nonlocal metadata
        label = f"{stage}-{round_number:02d}-{case}-{side}"
        location = args.output / label
        location.mkdir()
        output = location / "sample.json"
        env = dict(os.environ, SCODE_TEST_BACKEND="mock", SCODE_TEST_BIN=str(binary.resolve()),
                   SUDOCODE_EXPERIMENT_PROSE_PREVIEW=args.preview, SCODE_RENDER_CASE=case,
                   SCODE_RENDER_POLICY=str(args.policy.resolve()), SCODE_RENDER_REPORT=str(output.resolve()),
                   SCODE_TERMINAL_LOG_DIR=str(location.resolve()))
        print(label, flush=True)
        with (location / "driver.log").open("w") as log:
            result = subprocess.run([str(args.driver.resolve()), "--ignored", "--exact",
                                     "render_performance_workload", "--nocapture"],
                                    env=env, stdout=log, stderr=subprocess.STDOUT,
                                    timeout=120 + 15 * policy["cases"][case]["turns"])
        assert result.returncode == 0, f"{label}: workload failed; see {location / 'driver.log'}"
        value = json.loads(output.read_text())
        validate(value, case, policy)
        assert value["preview"] == args.preview, "experiment state changed"
        if metadata is None:
            metadata = value["metadata"]
        assert metadata == value["metadata"], "terminal observer/platform changed"
        report["samples"].append({"stage": stage, "round": round_number, "side": side,
                                  "path": str(output.relative_to(args.output)), "data": value})
        save()
        return value

    try:
        # Finish compilation before invocation. Warm both binaries before collecting pairs.
        for case in cases:
            sample(args.base, case, "warm", 0, "base")
            sample(args.candidate, case, "warm", 0, "candidate")
        for stage, rounds, collection in [
            ("aa", policy["rounds"]["calibration"], controls),
            ("ab", policy["rounds"]["comparison"], comparisons),
        ]:
            for number in range(rounds):
                # Reverse both binary and scenario order to reduce thermal/time drift.
                for case in cases if number % 2 == 0 else list(reversed(cases)):
                    pair = {}
                    for side in ["base", "candidate"] if number % 2 == 0 else ["candidate", "base"]:
                        binary = args.base if stage == "aa" or side == "base" else args.candidate
                        pair[side] = sample(binary, case, stage, number, side)
                    collection[case].append((pair["base"], pair["candidate"]))
            if stage == "aa":
                report["calibration"], noise_failures = assess(controls, None, policy, cases)
                if noise_failures:
                    report.update(status="noisy", failures=noise_failures, metrics=report["calibration"])
                    break
        if report["status"] == "running":
            report["metrics"], report["failures"] = assess(controls, comparisons, policy, cases)
            report["status"] = "failed" if report["failures"] else "passed"
    except (AssertionError, OSError, ValueError, KeyError, TypeError, subprocess.TimeoutExpired) as error:
        report["status"] = "invalid"
        report["failures"] = [str(error)]
    finally:
        save()
    lines = ["## Rendering performance", "", f"Result: **{report['status']}**", "",
             f"Profile: `{args.profile}` · preview: `{args.preview}`", ""]
    for failure in report.get("failures", []):
        lines.append(f"- {failure}")
    if report.get("metrics"):
        lines.extend(["", "| Case | Metric | Baseline | Candidate | Result |",
                      "| --- | --- | ---: | ---: | --- |"])
        for row in report["metrics"]:
            baseline = f"{row['baseline']:.2f}" if "baseline" in row else "—"
            candidate = f"{row['candidate']:.2f}" if "candidate" in row else "—"
            lines.append(f"| {row['case']} | {row['metric']} | {baseline} | {candidate} | {row['result']} |")
    summary = "\n".join(lines) + "\n"
    (args.output / "summary.md").write_text(summary)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as stream:
            stream.write(summary)
    print(summary)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
