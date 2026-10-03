"""Bound the live PTY sweep and reject incomplete or empty sweep reports."""

import collections
import json
import os
from pathlib import Path
import re
import sys

SHARD_SIZE = 8  # Eight 90-second process deadlines fit in a 15-minute test step.


def plan():
    manual = os.environ.get("MANUAL_MODELS", "").strip()
    if manual:
        models = [model.strip() for model in manual.split(",") if model.strip()]
    else:
        catalog = json.load(sys.stdin)
        non_chat = re.compile(
            r"embed|tts|whisper|dall-e|moderation|search|image|video|speech|audio|transcribe|seedance",
            re.IGNORECASE,
        )
        models = [
            item["id"] for item in catalog["data"]
            if item.get("id") and not non_chat.search(item["id"])
        ]
    models = sorted(set(models))
    if not models:
        raise SystemExit("No chat models discovered")
    matrix = {"include": [
        {"shard": index // SHARD_SIZE, "models": ",".join(models[index:index + SHARD_SIZE])}
        for index in range(0, len(models), SHARD_SIZE)
    ]}
    print(f"Discovered {len(models)} models in {len(matrix['include'])} shards")
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
        output.write(f"matrix={json.dumps(matrix, separators=(',', ':'))}\n")


def collect(directory):
    matrix = json.loads(os.environ["COMPAT_MATRIX"])
    expected = [model for shard in matrix["include"] for model in shard["models"].split(",")]
    Path(directory).mkdir(parents=True, exist_ok=True)
    reports = list(Path(directory).glob("model-compat-[0-9]*.json"))
    rows = []
    errors = []
    if len(reports) != len(matrix["include"]):
        errors.append(f"Expected {len(matrix['include'])} reports, received {len(reports)}")
    for path in sorted(reports):
        report = json.loads(path.read_text(encoding="utf-8"))
        if report["completed"] != report["total"]:
            errors.append(f"{path.name}: only {report['completed']}/{report['total']} completed")
        rows.extend(report["models"])
    if collections.Counter(row["model"] for row in rows) != collections.Counter(expected):
        errors.append("Reported models do not match the discovery plan")
    counts = collections.Counter(row["status"] for row in rows)
    if set(counts) - {"PASS", "SKIP", "REFUSED", "FAIL"}:
        errors.append("Unknown result status")
    if counts["FAIL"]:
        errors.append(f"{counts['FAIL']} compatibility failures")
    if not counts["PASS"]:
        errors.append("No model passed")
    report = {"total": len(expected), "completed": len(rows),
              "pass": counts["PASS"], "skip": counts["SKIP"], "fail": counts["FAIL"],
              "refused": counts["REFUSED"],
              "errors": errors, "models": rows}
    output = Path(directory) / "model-compat-report.json"
    output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"{counts['PASS']} pass, {counts['SKIP']} skip, "
          f"{counts['REFUSED']} refused, {counts['FAIL']} fail")
    if errors:
        raise SystemExit("; ".join(errors))


if __name__ == "__main__":
    if sys.argv[1] == "plan":
        plan()
    elif sys.argv[1] == "collect":
        collect(sys.argv[2])
    else:
        raise SystemExit("Expected plan or collect")
