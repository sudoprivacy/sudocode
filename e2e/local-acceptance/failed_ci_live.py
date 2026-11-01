"""Exercise the actual failure-only workflow on an isolated GitHub branch.

Write the dispatch fixture on a disposable branch, commit and push with the
normal hook, then run --ref BRANCH --output NEW_DIRECTORY. Never merge the
fixture branch. No production source, daemon, provider key or runner is used by
the two controlled jobs. Ordinary CI preflight remains present on that branch.
"""
import argparse
import base64
from datetime import datetime, timezone
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time

ROOT = Path(__file__).resolve().parents[2]
REPOSITORY = "sudoprivacy/sudocode"
WORKFLOW = ".github/workflows/rust-ci.yml"
CANCELLER = ".github/workflows/ci-cancel-failed-run.yml"


def callers():
    """Retain each production dependency and failure predicate in the live field."""
    workflow = (ROOT / WORKFLOW).read_text(encoding="utf-8")
    selected = []
    for source, dependency in [("clippy", "clippy-workspace"), ("daemon", "a2a-live")]:
        match = re.search(rf"^  stop-after-{source}-failure:\n.*?(?=^  [a-z]|\Z)", workflow, re.M | re.S)
        assert match, f"Missing independent {source} failure caller"
        block = match.group()
        assert f"needs: {dependency}\n" in block
        assert f"if: always() && github.event_name == 'merge_group' && needs.{dependency}.result == 'failure'" in block
        assert f"uses: ./{CANCELLER}" in block and "actions: write" in block
        selected.append(block.replace(f"  stop-after-{source}-failure:", f"  fixture-stop-after-{source}-failure:")
            .replace("github.event_name == 'merge_group'", "inputs.failure_fixture != ''")
            .replace(dependency, f"fixture-{source}"))
    helper = (ROOT / CANCELLER).read_text(encoding="utf-8")
    assert "checkout@" not in helper and "secrets: inherit" not in helper
    assert 'repos/$env:GITHUB_REPOSITORY/actions/runs/$env:GITHUB_RUN_ID/cancel' in helper
    assert "force-cancel" not in helper
    return workflow, selected


def write_fixture(destination):
    workflow, selected = callers()
    assert "failure_fixture:" not in workflow, "Fixture already present; preserve the original source"
    anchor = "      live_only:\n"
    assert workflow.count(anchor) == 1
    workflow = workflow.replace(anchor,
        "      failure_fixture:\n        description: 'Controlled fail-fast field; never merge this fixture branch'\n"
        "        type: string\n        default: ''\n" + anchor)
    for source, opposite in [("clippy", "daemon"), ("daemon", "clippy")]:
        matrix = ("    strategy:\n      fail-fast: true\n      matrix:\n        lane: [first, peer]\n"
                  if source == "clippy" else "")
        suffix = " (${{ matrix.lane }})" if source == "clippy" else ""
        lane = "          LANE: ${{ matrix.lane }}\n" if source == "clippy" else ""
        workflow += f"""
  fixture-{source}:
    name: Fail-fast fixture {source}{suffix}
    if: github.event_name == 'workflow_dispatch' && inputs.failure_fixture != ''
    runs-on: ubuntu-latest
    timeout-minutes: 5
{matrix}    defaults:
      run:
        working-directory: .
    steps:
      - name: Controlled result with an independently running peer
        env:
          MODE: ${{{{ inputs.failure_fixture }}}}
{lane}        run: |
          case "$MODE" in
            {source})
              if [ "${{LANE:-first}}" = first ]; then
                sleep 20; echo 'Controlled fixture failure'; exit 23
              else
                sleep 180
              fi ;;
            {opposite}) echo 'Peer must be cancelled before its 180-second work completes'; sleep 180 ;;
            clean) sleep 20 ;;
            *) exit 24 ;;
          esac
"""
    gate = re.search(r"^  integrated-acceptance:\n.*?(?=^  a2a-live:)", workflow, re.M | re.S)
    assert gate, "Retain the actual required gate in the isolated field"
    workflow += "\n" + gate.group().replace("  integrated-acceptance:", "  fixture-integrated-acceptance:") \
        .replace("github.event_name == 'merge_group'", "github.event_name == 'workflow_dispatch' && inputs.failure_fixture != ''") \
        .replace("clippy-workspace", "fixture-clippy").replace("a2a-live", "fixture-daemon") \
        .replace("name: nexus-A2A live (real daemon)", "name: Fixture required acceptance")
    workflow += "\n" + "\n".join(selected)
    destination.write_text(workflow.rstrip() + "\n", encoding="utf-8")
    print(f"Wrote isolated dispatch fixture: {destination}. Commit/push with the normal hook; never merge it.")


def gh(*arguments):
    return subprocess.check_output(["gh", *arguments], cwd=ROOT)


def api(path):
    return json.loads(gh("api", f"repos/{REPOSITORY}/{path}"))


def live(ref, output):
    callers()
    assert re.fullmatch(r"ci/fail-fast-probe-[a-zA-Z0-9-]+", ref), "Use a dedicated ci/fail-fast-probe-* branch"
    output.mkdir(parents=True, exist_ok=False)
    head = api(f"git/ref/heads/{ref}")["object"]["sha"]
    helper = base64.b64decode(api(f"contents/{CANCELLER}?ref={ref}")["content"])
    assert helper == (ROOT / CANCELLER).read_text(encoding="utf-8").encode(), "Field must execute the current cancellation bytes"
    helper_hash = hashlib.sha256(helper).hexdigest()
    records = []
    for mode in ["clippy", "daemon", "clean"]:
        before = {r["id"] for r in api(f"actions/workflows/rust-ci.yml/runs?branch={ref}&event=workflow_dispatch")["workflow_runs"]}
        gh("workflow", "run", "rust-ci.yml", "--repo", REPOSITORY, "--ref", ref, "-f", f"failure_fixture={mode}")
        print(f"Dispatched real {mode} field for {head}", flush=True)
        started, run = time.monotonic(), None
        while time.monotonic() - started < 900:
            runs = api(f"actions/workflows/rust-ci.yml/runs?branch={ref}&event=workflow_dispatch")["workflow_runs"]
            fresh = [r for r in runs if r["id"] not in before and r["head_sha"] == head]
            assert len(fresh) <= 1, "Another controller dispatched this private branch; preserve evidence"
            if fresh:
                run = fresh[0]
                (output / f"{mode}-run.json").write_text(json.dumps(run, indent=2))
                if run["status"] == "completed":
                    break
            time.sleep(10)
        assert run and run["status"] == "completed", "Field did not complete; retained run receipt identifies owned work"
        jobs = api(f"actions/runs/{run['id']}/jobs?per_page=100")["jobs"]
        (output / f"{mode}-jobs.json").write_text(json.dumps(jobs, indent=2))
        producers = {kind: [j for j in jobs if j["name"].startswith(f"Fail-fast fixture {kind}")]
                     for kind in ["clippy", "daemon"]}
        assert len(producers["clippy"]) == 2 and len(producers["daemon"]) == 1, producers
        required = next(j for j in jobs if j["name"] == "Fixture required acceptance")
        record = {"mode": mode, "run": run["id"], "head": head, "conclusion": run["conclusion"]}
        if mode == "clean":
            assert run["conclusion"] == "success", run["html_url"]
            assert all(j["conclusion"] == "success" for group in producers.values() for j in group)
            assert required["conclusion"] == "success", required
            stops = [j for j in jobs if "fixture-stop-after-" in j["name"]]
            assert len(stops) == 2 and all(j["conclusion"] == "skipped" for j in stops), stops
        else:
            failed = next(j for j in producers[mode] if j["conclusion"] == "failure")
            peers = producers["daemon" if mode == "clippy" else "clippy"]
            assert all(j["conclusion"] == "cancelled" for j in peers), producers
            assert required["conclusion"] == "failure", required
            assert any(s["conclusion"] == "failure" for s in failed["steps"]), failed
            assert run["conclusion"] in {"failure", "cancelled"}, run
            durations = []
            for peer in peers:
                work = next(s for s in peer["steps"] if s["name"] == "Controlled result with an independently running peer")
                assert work["started_at"] and work["conclusion"] == "cancelled", "The peer's actual work must start, not only be queued"
                duration = (datetime.fromisoformat(peer["completed_at"].replace("Z", "+00:00"))
                            - datetime.fromisoformat(peer["started_at"].replace("Z", "+00:00"))).total_seconds()
                assert duration < 180, "No saving: the independent peer ran its entire original workload"
                durations.append(duration)
            record["cancelled_peer_seconds"] = durations
        records.append(record)
        (output / "result.json").write_text(json.dumps({"status": "running", "cases": records}, indent=2))
        print(json.dumps(record), flush=True)
    result = {"status": "PASS", "observed_at": datetime.now(timezone.utc).isoformat(), "cases": records,
              "cancellation_sha256": helper_hash,
              "scope": "Real GitHub matrix, unchanged cancellation bytes and required gate shell. Production callers use dispatch/fixture dependency names in this isolated field. Both failure directions retain the original failed step and required gate failure; a green run allocates no cancellation job. Controlled jobs prove scheduling behavior, not production time savings or product journey coverage."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check-wiring", action="store_true")
    parser.add_argument("--write-fixture", type=Path)
    parser.add_argument("--ref")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if args.check_wiring:
        callers()
        print("PASS: independent failure callers retain exact-run cancellation and no candidate checkout/write token")
    elif args.write_fixture:
        write_fixture(args.write_fixture.resolve())
    else:
        assert args.ref and args.output, "Supply --ref and a fresh --output directory"
        live(args.ref, args.output.resolve())
