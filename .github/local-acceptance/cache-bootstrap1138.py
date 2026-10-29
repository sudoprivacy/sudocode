"""One reviewed four-file contract bootstrap; never a general acceptance bypass."""
import ast
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys

REPOSITORY = "sudoprivacy/sudocode"
PR = 1138
HEAD = "d035997c7ad58308b10c6c578ce711fb4892e005"
BASE = "893bc7e8ec260881897ed6640f651eed9ccff787"
REVIEW = 5704992617
ADMINISTRATOR = "elfenlieds7"
BLOBS = {
    ".github/local-acceptance/checks.py": "3b3c3276ebc57daa1887de41574a03db9e827916",
    ".github/scripts/local_acceptance.py": "24b3a9a08080ed0fca8ababd1d96bc9fb56af8b7",
    "e2e/local-acceptance/cache_acceptance.py": "4b3cbac832ca332c7c39851e077c6d471bfa95c8",
    "e2e/local-acceptance/run.py": "2b0aad4b10a12c8a04697a2162e7d9465970c40b",
}
ROOT = Path.cwd()


def output(command, **kwargs):
    """Decode Git/GitHub UTF-8 independently of the Windows console code page."""
    return subprocess.check_output(command, text=True, encoding="utf-8", **kwargs)


def git(*args):
    return output(["git", *args], cwd=ROOT).strip()


def api(path, body=None):
    command = ["gh", "api", "repos/" + REPOSITORY + "/" + path]
    if body is not None:
        command += ["--method", "POST", "--input", "-"]
    return json.loads(output(command, input=None if body is None else json.dumps(body)))


def pages(path, field=None):
    rows = []
    for number in range(1, 11):
        value = api(path + ("&" if "?" in path else "?") + f"per_page=100&page={number}")
        batch = value[field] if field else value
        rows.extend(batch)
        if len(batch) < 100:
            return rows
    raise RuntimeError("Bootstrap metadata pagination exceeded its bound")


def queue_entry():
    query = ('query { repository(owner:"sudoprivacy",name:"sudocode") { pullRequest(number:1138) '
             '{ mergeQueueEntry { headCommit { oid } baseCommit { oid } } } } }')
    value = json.loads(output(["gh", "api", "graphql", "-f", "query=" + query]))
    assert not value.get("errors"), value
    return value["data"]["repository"]["pullRequest"]["mergeQueueEntry"]


def main():
    assert os.environ["GITHUB_REPOSITORY"] == REPOSITORY
    assert os.environ["GITHUB_ACTOR"] == ADMINISTRATOR
    assert git("rev-parse", "HEAD") == BASE, "Execute only the frozen protected baseline"
    pull = api(f"pulls/{PR}")
    assert pull["state"] == "open" and not pull["draft"] and pull["head"]["sha"] == HEAD
    assert pull["head"]["repo"]["full_name"] == REPOSITORY
    review = api(f"pulls/{PR}/reviews/{REVIEW}")
    assert review["user"]["login"] == ADMINISTRATOR and review["commit_id"] == HEAD
    assert review["state"] in ["COMMENTED", "APPROVED"] and "<!-- approved-four-file-acceptance-bootstrap -->" in review["body"]
    assert api(f"collaborators/{ADMINISTRATOR}/permission")["permission"] == "admin"
    subprocess.run(["git", "fetch", "--no-tags", "origin", HEAD,
                    "+refs/notes/local-acceptance:refs/notes/local-acceptance"], check=True)
    assert git("diff", "--name-only", BASE, HEAD).splitlines() == sorted(BLOBS)
    for path, blob in BLOBS.items():
        assert git("rev-parse", HEAD + ":" + path) == blob and blob in review["body"], path

    # Import only protected code. Candidate files are read as data, never executed
    # in this write-enabled operation. The candidate-policy receipt is signed by
    # an already-enrolled device, and every changed byte is bound to the review.
    spec = importlib.util.spec_from_file_location("acceptance", ROOT / ".github/scripts/local_acceptance.py")
    acceptance = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(acceptance)
    receipt = acceptance.verify(HEAD, BASE, HEAD)
    assert receipt["host"]["system"] == "macos" and set(receipt["checks"]) == {"preflight", "guard-contract"}
    assert [r["command"][1:] for r in receipt["checks"]["preflight"]] == [
        [".github/scripts/check_doc_source_of_truth.py"],
        [".github/scripts/check_pr_history.py", BASE, "HEAD"],
    ]
    assert [r["command"][1:] for r in receipt["checks"]["guard-contract"]] == [["e2e/local-acceptance/run.py"]]
    def function(ref, name):
        tree = ast.parse(git("show", ref + ":.github/local-acceptance/checks.py"))
        return next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == name)
    assert ast.dump(function(BASE, "run")) == ast.dump(function(HEAD, "run"))
    assert git("rev-parse", BASE + ":spec/local_acceptance_signers.json") == git("rev-parse", HEAD + ":spec/local_acceptance_signers.json")

    kind = os.environ["BOOTSTRAP_TARGET_KIND"]
    assert kind in ["pull_request", "merge_group"]
    target = HEAD
    if kind == "merge_group":
        entry = queue_entry()
        assert entry, "The exact correction must be in the normal queue"
        target, queue_base = entry["headCommit"]["oid"], entry["baseCommit"]["oid"]
        assert all(re.fullmatch("[a-f0-9]{40}", value) for value in [target, queue_base])
        subprocess.run(["git", "fetch", "--no-tags", "origin", target, queue_base], check=True)
        subprocess.run(["git", "merge-base", "--is-ancestor", HEAD, target], check=True)
        assert git("diff", "--name-only", queue_base, target).splitlines() == sorted(BLOBS)
        for path, blob in BLOBS.items():
            assert git("rev-parse", target + ":" + path) == blob, path
        assert acceptance.commands_hash(queue_base) == acceptance.commands_hash(BASE)
        subprocess.run([sys.executable, ".github/scripts/check_pr_history.py", queue_base, target], check=True)

    runs = pages(f"actions/runs?head_sha={target}&event={kind}", "workflow_runs")
    latest = {}
    for run in runs:
        latest.setdefault(run["path"].split("@")[0], run)
    for path, run in latest.items():
        if path == ".github/workflows/local-acceptance.yml":
            continue
        assert run["status"] == "completed" and run["conclusion"] == "success", (path, run["status"], run["conclusion"])
    assert ".github/workflows/rust-ci.yml" in latest
    ordinary = latest[".github/workflows/local-acceptance.yml"]
    assert ordinary["status"] == "completed" and ordinary["conclusion"] == "failure"
    jobs = pages(f"actions/runs/{ordinary['id']}/jobs", "jobs")
    proof = next(j for j in jobs if j["name"] == "Verify local acceptance")
    assert proof["conclusion"] == "failure"
    assert all(j["conclusion"] == "skipped" or j["id"] == proof["id"] for j in jobs)
    log = output(["gh", "run", "view", str(ordinary["id"]), "--repo", REPOSITORY,
                  "--job", str(proof["id"]), "--log-failed"])
    assert "No current valid local acceptance: Acceptance commands changed; rerun local acceptance" in log
    prior = next(s for s in pages(f"commits/{target}/statuses") if s["context"] == "Local acceptance")
    assert prior["state"] == "failure" and prior["creator"]["login"] == "github-actions[bot]"
    prefix = f"https://github.com/{REPOSITORY}/actions/runs/"
    assert prior["target_url"].startswith(prefix)
    publisher_id = prior["target_url"][len(prefix):]
    assert publisher_id.isdecimal()
    publisher = api("actions/runs/" + publisher_id)
    assert publisher["path"].split("@")[0] == ".github/workflows/local-acceptance-publish.yml"
    assert publisher["status"] == "completed" and publisher["conclusion"] == "failure"
    current = api(f"pulls/{PR}")
    assert current["state"] == "open" and current["head"]["sha"] == HEAD
    if kind == "merge_group":
        assert queue_entry()["headCommit"]["oid"] == target
    evidence = dict(pr=PR, approved_head=HEAD, target=target, kind=kind,
                    signed_checks=list(receipt["checks"]), native_profile_unchanged=True,
                    ordinary_tests_passed=True, sole_refusal="reviewed command-hash change",
                    branch_protection_changed=False)
    if "--check-only" not in sys.argv:
        result = api("statuses/" + target, dict(state="success", context="Local acceptance",
            description="Reviewed four-file bootstrap; signed guard verified",
            target_url=prefix + os.environ["GITHUB_RUN_ID"]))
        assert result["state"] == "success" and result["creator"]["login"] == "github-actions[bot]"
        evidence["bootstrap_status"] = result["id"]
    print(json.dumps(evidence))


if __name__ == "__main__":
    main()
