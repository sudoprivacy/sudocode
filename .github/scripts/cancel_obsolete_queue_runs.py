"""Cancel runs for removed merge-queue refs; default to a read-only preview."""

import argparse
import datetime as dt
import json
from pathlib import Path
import subprocess
from urllib.parse import quote


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
        if missing_ok and "HTTP 404" in result.stderr:
            return None
        raise RuntimeError(result.stderr or result.stdout)
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
            if current["status"] == "completed":
                continue
            if current["event"] != "merge_group" or current["head_sha"] != run["head_sha"]:
                raise RuntimeError("Run identity changed during cleanup")
            item = {"id": run["id"], "workflow": run["name"], "head": run["head_sha"],
                    "reason": "Queue ref removed and commit absent from current queue", "cancel_requested": False}
            report["runs"].append(item)
            if args.cancel:
                request(f"{prefix}/actions/runs/{run['id']}/cancel", {})
                item["cancel_requested"] = True
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"eligible_runs": len(report["runs"]), "cancellations_requested":
              sum(run["cancel_requested"] for run in report["runs"]), "output": str(args.output)}))


if __name__ == "__main__":
    main()
