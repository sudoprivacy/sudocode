"""Drive the actual maintenance CLI across deterministic GitHub process races.

Only the GitHub subprocess is scripted. No real workflow is cancelled, and
these infrastructure fixtures do not claim product or provider acceptance.
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


SOURCE = Path(__file__).resolve().parents[2]
HEAD = "1" * 40


def github_child():
    path = Path(os.environ["SCODE_QUEUE_FIXTURE_STATE"])
    state = json.loads(path.read_text())
    args = sys.argv[1:]
    assert args[0] == "api", args
    endpoint, case = args[1], state["case"]
    state["calls"].append(endpoint)
    path.write_text(json.dumps(state))
    run = {"id": 42, "head_sha": HEAD, "head_branch": "gh-readonly-queue/main/pr-1-fixture",
           "name": "Rust CI", "event": "merge_group", "status": "in_progress",
           "created_at": "2000-01-01T00:00:00Z", "conclusion": None}

    def error(code):
        print(f"gh: fixture response (HTTP {code})", file=sys.stderr)
        raise SystemExit(1)

    if endpoint == "graphql":
        if case == "queue-lookup-forbidden":
            error(403)
        current = case == "live-queue" or (case == "queue-reappeared" and state["calls"].count("graphql") > 1)
        response = {"data": {"repository": {"mergeQueue": {"entries": {
            "nodes": [{"headCommit": {"oid": HEAD}}] if current else []}}}}}
    elif "/actions/runs?" in endpoint:
        response = {"workflow_runs": [run] if "status=in_progress&" in endpoint else []}
    elif "/git/ref/" in endpoint:
        if case != "ref-retained":
            error(404)
        response = {"object": {"sha": HEAD}}
    elif endpoint.endswith("/cancel"):
        assert "--method" in args and args[args.index("--method") + 1] == "POST"
        assert json.loads(sys.stdin.read()) == {}
        if case.startswith("conflict-") or case == "completion-race":
            error(409)
        if case in {"cancel-forbidden", "cancel-missing", "cancel-server-error"}:
            error({"cancel-forbidden": 403, "cancel-missing": 404, "cancel-server-error": 500}[case])
        response = {}
    elif endpoint.endswith("/actions/runs/42"):
        following_conflict = any(item.endswith("/cancel") for item in state["calls"])
        if case == "already-completed" or (following_conflict and case != "conflict-running"):
            run.update(status="completed", conclusion="success")
        if following_conflict and case.startswith("conflict-wrong-"):
            field = case.removeprefix("conflict-wrong-")
            run[field] = {"id": 43, "head_sha": "2" * 40, "head_branch": "other-branch",
                          "event": "pull_request"}[field]
        response = run
    else:
        raise AssertionError(endpoint)
    print(json.dumps(response))


def main():
    if os.environ.get("SCODE_QUEUE_GITHUB_CHILD") == "1":
        github_child()
        return
    if os.name == "nt":
        raise RuntimeError("Queue maintenance runs on Ubuntu; this subprocess fixture requires a POSIX executable shim")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--subject", type=Path, default=SOURCE / ".github/scripts/cancel_obsolete_queue_runs.py")
    subject = parser.parse_args().subject.resolve()
    cases = [("cancel-accepted", 0, 1), ("completion-race", 0, 1),
             ("already-completed", 0, 0), ("live-queue", 0, 0), ("queue-reappeared", 0, 0),
             ("ref-retained", 0, 0), ("preview-only", 0, 0), ("conflict-running", 1, 1),
             ("cancel-forbidden", 1, 1), ("cancel-missing", 1, 1), ("cancel-server-error", 1, 1),
             ("queue-lookup-forbidden", 1, 0)]
    cases += [("conflict-wrong-" + field, 1, 1) for field in ["id", "head_sha", "head_branch", "event"]]
    with tempfile.TemporaryDirectory(prefix="scode-queue-maintenance-") as temporary:
        root = Path(temporary)
        shim = root / "gh"
        shim.write_text(f"#!{sys.executable}\nimport os,runpy\nos.environ['SCODE_QUEUE_GITHUB_CHILD']='1'\n"
                        f"runpy.run_path({str(Path(__file__).resolve())!r}, run_name='__main__')\n")
        shim.chmod(0o755)
        for case, expected, mutations in cases:
            state, output = root / (case + ".json"), root / (case + "-report.json")
            state.write_text(json.dumps({"case": case, "calls": []}))
            environment = {**os.environ, "PATH": str(root) + os.pathsep + os.environ["PATH"],
                           "SCODE_QUEUE_FIXTURE_STATE": str(state)}
            environment.pop("SCODE_QUEUE_GITHUB_CHILD", None)
            command = [sys.executable, str(subject),
                       "--repository", "fixture/queue", "--output", str(output)]
            if case != "preview-only":
                command.append("--cancel")
            result = subprocess.run(command, env=environment, capture_output=True, text=True)
            assert (result.returncode != 0) == bool(expected), (case, result.stdout, result.stderr)
            calls = json.loads(state.read_text())["calls"]
            assert sum(item.endswith("/cancel") for item in calls) == mutations, (case, calls)
            if expected:
                assert "Run identity changed" in result.stderr or "HTTP " in result.stderr, (case, result.stderr)
            elif output.exists():
                rows = json.loads(output.read_text())["runs"]
                assert sum(row["cancel_requested"] for row in rows) == (case == "cancel-accepted"), (case, rows)
                if case == "completion-race":
                    assert len(rows) == 1 and rows[0]["completed_before_cancel"] is True
                    assert rows[0]["status_after_conflict"] == "completed" and rows[0]["conclusion"] == "success"
            print(f"PASS: {case}; actual CLI exit {result.returncode}, cancellation attempts {mutations}")


if __name__ == "__main__":
    main()
