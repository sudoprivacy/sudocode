"""One painted-terminal functional pass: real browser links, selection and idle output."""
import argparse
import json
import math
import os
from pathlib import Path
import subprocess
import sys
from benchmark import digest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ["driver", "candidate", "output"]:
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--expected-candidate-sha256", default=os.environ.get("SCODE_EXPECTED_CANDIDATE_SHA256"))
    args = parser.parse_args()
    if args.output.exists() and any(args.output.iterdir()):
        parser.error("choose an empty output directory to preserve previous evidence")
    args.output.mkdir(parents=True, exist_ok=True)
    sampling = json.loads(Path(__file__).with_name("gui-policy.json").read_text())["sampling"]
    keys = len(sampling["input_delays_ms"])
    sample_counts = {"drag_frame_ms": sampling["drag_steps"]}
    for phase in ["history", "wait", "stream"]:
        sample_counts[f"input_{phase}_parsed_ms"] = keys * sampling["parsed_bursts"]
        sample_counts[f"input_{phase}_frame_ms"] = keys * sampling["painted_bursts"]
    sample = args.output / "sample.json"
    env = dict(os.environ, SCODE_TEST_BIN=str(args.candidate.resolve()), SCODE_TEST_BACKEND="mock",
               SCODE_GUI_REPORT=str(sample.resolve()), SCODE_TERMINAL_LOG_DIR=str(args.output.resolve()),
               SCODE_GUI_EXPECT_LINKS="1")
    report = {"version": 1, "status": "running", "candidate_sha256": digest(args.candidate),
              "driver_sha256": digest(args.driver), "expected_candidate_sha256": args.expected_candidate_sha256}
    try:
        assert (args.expected_candidate_sha256 is None
                or args.expected_candidate_sha256 == report["candidate_sha256"]), "candidate differs from the validated build"
        with (args.output / "driver.log").open("w") as log:
            result = subprocess.run([str(args.driver.resolve()), "--ignored", "--exact",
                                     "browser_terminal_links_and_interaction", "--nocapture"],
                                    env=env, stdout=log, stderr=subprocess.STDOUT, timeout=180)
        assert result.returncode == 0, f"browser terminal run failed: {args.output / 'driver.log'}"
        data = json.loads(sample.read_text())
        assert data["status"] == "passed" and data["idle_bytes"] == 0, "invalid browser run"
        assert data["streaming_selection_preserved"], "missing streaming selection acceptance"
        assert set(data["samples"]) == set(sample_counts), "missing interaction workload"
        for name, values in data["samples"].items():
            assert len(values) == sample_counts[name], "missing interaction samples"
            assert all(math.isfinite(value) and value >= 0 for value in values), "invalid interaction sample"
        assert digest(args.candidate) == report["candidate_sha256"], "candidate changed during the run"
        report.update(status="passed", browser=data["browser"], xterm=data["xterm"])
    except (AssertionError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        report.update(status="failed", failures=[str(error)])
    (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Browser terminal smoke: {report['status']}", *report.get("failures", []), sep="\n")
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
