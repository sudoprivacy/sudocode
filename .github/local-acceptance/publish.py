"""Publish a required status from protected code after read-only candidate CI."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import urllib.request


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("gate", ROOT / ".github/local-acceptance/gate.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def main():
    source = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())["workflow_run"]
    repository = os.environ["GITHUB_REPOSITORY"]
    run = gate.api(f"repos/{repository}/actions/runs/{source['id']}")
    if run["path"].split("@")[0] != ".github/workflows/local-acceptance.yml" or run["status"] != "completed":
        raise RuntimeError("Unexpected workflow identity or incomplete run")
    if source.get("run_attempt") != run["run_attempt"]:
        print("Old workflow attempt; publish nothing")
        return
    head = run["head_sha"]
    newer = gate.pages(f"repos/{repository}/actions/workflows/local-acceptance.yml/runs?head_sha={head}&event={run['event']}", "workflow_runs")
    if any(item["id"] > run["id"] for item in newer):
        print("A newer acceptance run owns this head; publish nothing")
        return
    if run["event"] == "pull_request":
        pulls = [pull for pull in gate.pages(f"repos/{repository}/pulls?state=open&base=main")
                 if pull["head"]["sha"] == head]
        if len(pulls) != 1:
            print("No unique current PR for this completed head; publish nothing")
            return
        if pulls[0]["draft"]:
            print("Draft PR; publish nothing")
            return
        event = {"pull_request": pulls[0]}
    elif run["event"] == "merge_group":
        gate.acceptance.git("fetch", "--no-tags", "origin", head)
        event = {"merge_group": {"head_sha": head, "base_sha": gate.acceptance.value("merge-base", "HEAD", head)}}
    else:
        raise RuntimeError("Unexpected workflow event")
    success = False
    error = "Source acceptance failed; inspect the acceptance workflow's jobs and responsibility plan"
    description = "Carried signed source acceptance; deployment is separate"
    try:
        # Revalidate even a failed run so the status exposes a missing receipt
        # or the exact failed platform rather than a generic red check.
        with tempfile.TemporaryDirectory(prefix="acceptance-event-") as directory:
            path = Path(directory) / "event.json"
            path.write_text(json.dumps(event))
            os.environ["GITHUB_EVENT_PATH"] = str(path)
            selection = gate.main()
        jobs = gate.pages(f"repos/{repository}/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs", "jobs")
        for name in selection["required_jobs"]:
            matching = [job for job in jobs if job["name"] == name]
            if len(matching) != 1 or matching[0]["conclusion"] != "success":
                raise RuntimeError(f"Required current-attempt job did not pass: {name}")
        if run["conclusion"] != "success":
            raise RuntimeError(error)
        if selection["plan"]:
            plan = selection["plan"]
            targets = dict.fromkeys(target for job in plan["ci"] for target in job["targets"])
            description = (f"Local {plan['author']['system']} + CI {','.join(targets) or 'none'} passed; "
                           "source only, deployment separate")
        success = True
    except (RuntimeError, OSError, ValueError, KeyError, subprocess.CalledProcessError) as problem:
        error = str(problem)
    body = {"state": "success" if success else "failure", "context": "Local acceptance",
            "description": (description if success else error)[:140],
            "target_url": f"https://github.com/{repository}/actions/runs/{os.environ['GITHUB_RUN_ID']}"}
    request = urllib.request.Request(f"https://api.github.com/repos/{repository}/statuses/{head}",
                                     data=json.dumps(body).encode(), method="POST",
                                     headers={"Authorization": "Bearer " + os.environ["GH_TOKEN"],
                                              "Content-Type": "application/json", "Accept": "application/vnd.github+json"})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)
    print(f"Published {result['state']} for {head}")
    if not success:
        raise RuntimeError(error)


if __name__ == "__main__":
    main()
