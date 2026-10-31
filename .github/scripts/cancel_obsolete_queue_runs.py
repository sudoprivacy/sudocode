"""Cancel runs for removed merge-queue refs; default to a read-only preview."""

import argparse
import datetime as dt
import json
from pathlib import Path
import re
import subprocess
from urllib.parse import quote


class GitHubRequestError(RuntimeError):
    """Preserve the CLI's HTTP status without treating arbitrary errors as conflicts."""

    def __init__(self, result):
        super().__init__(result.stderr or result.stdout)
        status = re.search(r"\(HTTP (\d{3})\)\s*$", result.stderr)
        self.status = int(status.group(1)) if status else None


def request(path, payload=None, missing_ok=False):
    """Use the runner's existing GitHub authentication without handling tokens."""
    args = ["gh", "api", path]
    if payload is not None:
        args += ["--method", "POST", "--input", "-"]
    result = subprocess.run(
        args, input=json.dumps(payload) if payload is not None else None,
        capture_output=True, text=True, encoding="utf-8",
    )
    if result.returncode:
        error = GitHubRequestError(result)
        if missing_ok and error.status == 404:
            return None
        raise error
    return json.loads(result.stdout) if result.stdout.strip() else {}


def queue_heads(owner, name):
    query = """query($owner:String!,$name:String!){repository(owner:$owner,name:$name){
      mergeQueue(branch:"main"){entries(first:100){nodes{headCommit{oid}}}}
    }}"""
    result = request("graphql", {"query": query, "variables": {"owner": owner, "name": name}})
    queue = result["data"]["repository"]["mergeQueue"]
    if queue is None:
        raise RuntimeError("Expected an active main merge queue")
    return {node["headCommit"]["oid"] for node in queue["entries"]["nodes"] if node["headCommit"]}


def verify_run_identity(actual, expected):
    """Require the same queue execution before acting on its current state."""
    if (actual["event"] != "merge_group" or any(actual[key] != expected[key]
            for key in ("id", "head_sha", "head_branch"))):
        raise RuntimeError("Run identity changed during cleanup")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--cancel", action="store_true", help="Apply the previewed cancellation policy")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    owner, name = args.repository.split("/")
    prefix = f"repos/{owner}/{name}"
    heads = queue_heads(owner, name)
    now = dt.datetime.now(dt.timezone.utc)
    report = {"repository": args.repository, "apply": args.cancel, "current_heads": sorted(heads), "runs": []}
    try:
        snapshot = {}
        for status in ("queued", "in_progress", "pending", "waiting", "requested"):
            page = 1
            while True:
                runs = request(f"{prefix}/actions/runs?event=merge_group&status={status}&per_page=100&page={page}")["workflow_runs"]
                snapshot.update({run["id"]: run for run in runs})
                if len(runs) < 100:
                    break
                page += 1
        # Collect pages before cancellations shrink the paginated result set.
        report["scanned_runs"] = len(snapshot)
        report["retained_current_head_runs"] = 0
        for run in snapshot.values():
            if run["head_sha"] in heads:
                report["retained_current_head_runs"] += 1
                continue
            if not run["head_branch"].startswith("gh-readonly-queue/main/"):
                continue
            created = dt.datetime.fromisoformat(run["created_at"].replace("Z", "+00:00"))
            if (now - created).total_seconds() < 120:
                continue
            ref = "heads/" + quote(run["head_branch"], safe="/")
            if request(f"{prefix}/git/ref/{ref}", missing_ok=True) is not None:
                continue
            # Recheck the live queue before each mutation. A lookup error fails
            # cleanup instead of treating a current run as stale.
            if run["head_sha"] in queue_heads(owner, name):
                continue
            current = request(f"{prefix}/actions/runs/{run['id']}")
            verify_run_identity(current, run)
            if current["status"] == "completed":
                continue
            item = {"id": run["id"], "workflow": run["name"], "head": run["head_sha"],
                    "reason": "Queue ref removed and commit absent from current queue", "cancel_requested": False}
            report["runs"].append(item)
            if args.cancel:
                try:
                    request(f"{prefix}/actions/runs/{run['id']}/cancel", {})
                    item["cancel_requested"] = True
                except GitHubRequestError as error:
                    if error.status != 409:
                        raise
                    # Completion can win the race after our last read. It is
                    # already the desired terminal state; never retry mutation.
                    finished = request(f"{prefix}/actions/runs/{run['id']}")
                    verify_run_identity(finished, run)
                    item["status_after_conflict"] = finished["status"]
                    if finished["status"] != "completed":
                        raise
                    item["completed_before_cancel"] = True
                    item["conclusion"] = finished["conclusion"]
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"eligible_runs": len(report["runs"]), "cancellations_requested":
              sum(run["cancel_requested"] for run in report["runs"]), "output": str(args.output)}))


if __name__ == "__main__":
    main()
