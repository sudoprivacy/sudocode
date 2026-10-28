"""Compare the opt-in viewport with the same release binary's normal REPL.

This additional workload uses the existing paired evaluator and budgets. The
browser observer also measures painted keys and browser resources. It does not
replace the protected rendering gate.
"""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import subprocess
import statistics
import sys

ROOT = Path(__file__).resolve().parents[1]
SHARED = ROOT / "e2e/terminal-resize"
spec = importlib.util.spec_from_file_location("render_benchmark", SHARED / "benchmark.py")
shared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shared)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--driver", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--observer", choices=["headless", "browser"], default="headless")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    core_policy = json.loads((SHARED / "performance-policy.json").read_text(encoding="utf-8"))
    gui_policy = json.loads((SHARED / "gui-policy.json").read_text(encoding="utf-8"))
    policy = core_policy if args.observer == "headless" else dict(gui_policy,
        rounds=core_policy["rounds"], contracts=core_policy["contracts"], sampling=core_policy["sampling"])
    headless_metrics = {"input_wait_p95_ms", "input_stream_p95_ms", "input_history_p95_ms",
               "cpu_ms", "rss_peak_mib", "rss_after_mib", "rss_growth_mib", "wire_kib"}
    metrics = headless_metrics if args.observer == "headless" else {
        "input_history_parsed_p95_ms", "input_history_frame_p95_ms", "terminal_cpu_ms",
        "terminal_rss_peak_mib", "terminal_rss_growth_mib"}
    comparison_policy = dict(policy, cases={"viewport": {}})
    controls, comparisons = {"viewport": []}, {"viewport": []}
    report = {"status": "running", "samples": [], "policy": policy, "observer": args.observer,
              "binary_sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
              "driver_sha256": hashlib.sha256(args.driver.read_bytes()).hexdigest(),
              "harness_sha256": {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest() for name in [
                  "scripts/benchmark_prompt_viewport.py",
                  "rust/crates/rusty-sudocode-cli/tests/support/prompt_viewport.cjs",
                  "rust/crates/rusty-sudocode-cli/tests/support/prompt_viewport_browser.cjs",
                  "e2e/terminal-resize/benchmark.py", "e2e/terminal-resize/host.cjs",
                  "e2e/terminal-resize/performance-policy.json",
                  "e2e/terminal-resize/gui-policy.json", "e2e/terminal-resize/gui.html",
                  "e2e/terminal-resize/resources.cjs",
              ]},
              "measurement": "Same binary, enabled/disabled. Headless CPU/output comparisons cover identical typing phases; extra scrolling and total costs remain in every sample. Browser painting includes two animation frames and PNG readback; RSS uses phase samples and per-process high-water values where available."}

    def sample(stage, number, side, enabled):
        location = args.output / f"{stage}-{number:02d}-{side}"
        env = dict(os.environ, SCODE_TEST_BIN=str(args.binary.resolve()),
                   SCODE_TEST_BACKEND="mock", SCODE_VIEWPORT_MEASURE_ENABLED=str(int(enabled)),
                   SCODE_VIEWPORT_MEASURE_DIR=str(location.resolve()))
        print(f"{location.name}: viewport={enabled}", flush=True)
        location.mkdir()
        with (location / "driver.log").open("w") as log:
            test = "viewport_terminal_measurements" if args.observer == "headless" else "viewport_browser_interaction"
            process = subprocess.run([str(args.driver.resolve()), "--ignored", "--exact", test, "--nocapture"],
                                     env=env, stdout=log, stderr=subprocess.STDOUT, timeout=180)
        if process.returncode:
            raise RuntimeError(f"{location.name}: workload failed; original trace retained")
        value = json.loads((location / "report.json").read_text(encoding="utf-8"))
        assert value["status"] == "complete" and value["enabled"] == enabled
        assert metrics <= value["metrics"].keys()
        assert all(isinstance(value["metrics"][key], (int, float)) and math.isfinite(value["metrics"][key])
                   and value["metrics"][key] >= 0 for key in metrics), "invalid measurement"
        if args.observer == "browser":
            assert len(value["samples"]) == policy["sampling"]["input"]
            assert len(value["frame_samples"]) == len(gui_policy["sampling"]["input_delays_ms"])
        else:
            assert value["contracts"] == policy["contracts"]
            for phase in ["wait", "stream", "history", "reading"]:
                assert len(value["samples"][phase]) == policy["sampling"]["input"]
        if enabled and args.observer == "headless":
            assert len(value["samples"]["scroll"]) == 20
            assert value["metrics"]["scroll_p95_ms"] <= policy["metrics"]["input_history_p95_ms"]["limit"], "scroll input exceeds the existing interaction budget"
            assert value["metrics"]["scroll_cpu_ms"] <= policy["metrics"]["cpu_ms"]["limit"], "scroll work exceeds the existing CPU budget"
            assert value["metrics"]["scroll_wire_kib"] <= policy["metrics"]["wire_kib"]["limit"], "scroll work exceeds the existing output budget"
        report["samples"].append({"stage": stage, "round": number, "side": side, "data": value})
        return value

    try:
        sample("warm", 0, "base", False)
        sample("warm", 0, "candidate", True)
        for stage, count, collection in [
            ("aa", policy["rounds"]["calibration"], controls),
            ("ab", policy["rounds"]["comparison"], comparisons),
        ]:
            for number in range(count):
                pair = {}
                for side in (["base", "candidate"] if number % 2 == 0 else ["candidate", "base"]):
                    pair[side] = sample(stage, number, side, stage == "ab" and side == "candidate")
                collection["viewport"].append((pair["base"], pair["candidate"]))
            if stage == "aa":
                report["calibration"], failures = shared.assess(controls, None, comparison_policy,
                                                               ["viewport"], lambda _: metrics)
                if failures:
                    report.update(status="noisy", failures=failures)
                    break
        if report["status"] == "running":
            rows, failures = shared.assess(controls, comparisons, comparison_policy,
                                          ["viewport"], lambda _: metrics)
            report.update(status="failed" if failures else "passed", metrics=rows, failures=failures)
    except (AssertionError, OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as error:
        report.update(status="invalid", failures=[str(error)])
    finally:
        (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    lines = ["## Inline prompt viewport", "", f"Result: **{report['status']}**", "",
             f"Same release binary, experiment disabled/enabled; observer: {args.observer}. Existing budgets are unchanged. Headless CPU/output comparisons use shared typing phases; browser CPU/memory cover the complete browser workflow.", ""]
    lines.extend(f"- {failure}" for failure in report.get("failures", []))
    if report.get("metrics"):
        lines.extend(["| Shared workload metric | Disabled | Enabled | Result |",
                      "| --- | ---: | ---: | --- |"])
        for row in report["metrics"]:
            lines.append(f"| {row['metric']} | {row['baseline']:.2f} | {row['candidate']:.2f} | {row['result']} |")
    final = {side: [sample["data"]["metrics"] for sample in report["samples"]
                    if sample["stage"] == "ab" and sample["side"] == side]
             for side in ["base", "candidate"]}
    if all(final.values()) and args.observer == "headless":
        lines.extend(["", "| Complete workload / additional interaction | Disabled | Enabled |",
                      "| --- | ---: | ---: |"])
        for metric in ["total_cpu_ms", "total_wire_kib", "scroll_cpu_ms", "scroll_wire_kib",
                       "input_reading_p95_ms"]:
            left = statistics.median(value[metric] for value in final["base"])
            right = statistics.median(value[metric] for value in final["candidate"])
            lines.append(f"| {metric} | {left:.2f} | {right:.2f} |")
        scroll = statistics.median(value["scroll_p95_ms"] for value in final["candidate"])
        lines.extend(["", f"Twenty wheel events: enabled scroll P95 **{scroll:.2f} ms**. The disabled REPL has no application history viewport."])
    summary = "\n".join(lines) + "\n"
    (args.output / "summary.md").write_text(summary, encoding="utf-8")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as stream:
            stream.write(summary)
    print(summary)
    print(json.dumps({"status": report["status"], "failures": report.get("failures", [])}))
    return int(report["status"] != "passed")


if __name__ == "__main__":
    sys.exit(main())
