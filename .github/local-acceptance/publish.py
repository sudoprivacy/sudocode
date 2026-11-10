"""Publish a required status from protected code after read-only candidate CI."""

import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import urllib.request


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("gate", ROOT / ".github/local-acceptance/gate.py")
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def applicable_run(run):
    """A draft snapshot never ran the required proof and cannot own readiness."""
    return run.get("display_title") != "Draft local acceptance" and run.get("conclusion") != "skipped"


def main():
    trigger = gate.github_event()
    repository = os.environ["GITHUB_REPOSITORY"]
    if "workflow_run" in trigger:
        source = trigger["workflow_run"]
    else:
        inputs = trigger.get("inputs", {})
        number, requested_head = str(inputs.get("pr", "")), inputs.get("head", "")
        if not number.isdecimal() or not re.fullmatch(r"[a-f0-9]{40}", requested_head):
            raise RuntimeError("Finalization requires a PR number and its exact frozen head")
        pull = gate.api(f"repos/{repository}/pulls/{number}")
        if pull["head"]["sha"] != requested_head or pull["state"] != "open" or pull["draft"]:
            raise RuntimeError("Finalization requires the current open ready PR head")
        candidates = gate.pages(f"repos/{repository}/actions/workflows/local-acceptance.yml/runs?head_sha={requested_head}&event=pull_request", "workflow_runs")
        candidates = [item for item in candidates if item["head_sha"] == requested_head and applicable_run(item)]
        source = max(candidates, key=lambda item: item["id"]) if candidates else None
        if not source or source["status"] != "completed":
            print("LOCAL_ACCEPTANCE_FINALIZATION: current source CI has not completed; its normal publisher "
                  "will independently verify the newly published author signature on completion. "
                  "No native job was restarted and no acceptance success was published.")
            return
    run = gate.api(f"repos/{repository}/actions/runs/{source['id']}")
    if run["path"].split("@")[0] != ".github/workflows/local-acceptance.yml" or run["status"] != "completed":
        raise RuntimeError("Unexpected workflow identity or incomplete run")
    if source.get("run_attempt") != run["run_attempt"]:
        print("Old workflow attempt; publish nothing")
        return
    if not applicable_run(run):
        print("Draft/skipped snapshot did not run required checks; publish no acceptance status. "
              "The applicable ready-for-review or merge-group run must finish.")
        return
    head = run["head_sha"]
    newer = gate.pages(f"repos/{repository}/actions/workflows/local-acceptance.yml/runs?head_sha={head}&event={run['event']}", "workflow_runs")
    owners = [item for item in newer if item["id"] > run["id"] and applicable_run(item)]
    if owners:
        print("A newer applicable acceptance run owns this head: "
              + ", ".join(str(item["id"]) for item in owners) + "; publish nothing")
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
    pending = False
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
        if run["event"] == "pull_request":
            description = description.replace("source only, deployment separate", "source only")
            description += f". When ready: gh pr merge {event['pull_request']['number']} --auto"
        pending = not selection.get("author_complete", True)
        if pending:
            description = "Native CI passed; full author signature still required. Finish the original local check; native jobs are retained."
            print("LOCAL_ACCEPTANCE_PENDING_AUTHOR: all required native CI jobs passed, but a scheduling plan "
                  "does not prove completed author checks. Complete/publish the original exact-head local "
                  "acceptance, then dispatch local-acceptance-publish.yml on main with pr/head. "
                  "Do not rerun native jobs or weaken the required Local acceptance status.")
        success = not pending
    except (RuntimeError, OSError, ValueError, KeyError, subprocess.CalledProcessError) as problem:
        error = str(problem)
    body = {"state": "success" if success else "pending" if pending else "failure", "context": "Local acceptance",
            "description": (description if success or pending else error)[:140],
            "target_url": f"https://github.com/{repository}/actions/runs/{os.environ['GITHUB_RUN_ID']}"}
    request = urllib.request.Request(f"https://api.github.com/repos/{repository}/statuses/{head}",
                                     data=json.dumps(body).encode(), method="POST",
                                     headers={"Authorization": "Bearer " + os.environ["GH_TOKEN"],
                                              "Content-Type": "application/json", "Accept": "application/vnd.github+json"})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)
    print(f"Published {result['state']} for {head}")
    if not success and not pending:
        raise RuntimeError(error)


if __name__ == "__main__":
    main()
