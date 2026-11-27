"""Trusted, read-only metadata gate; candidate sources are never executed here."""

import importlib.util
from datetime import datetime
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import urllib.parse
import urllib.request


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("acceptance", ROOT / ".github/scripts/local_acceptance.py")
acceptance = importlib.util.module_from_spec(spec)
spec.loader.exec_module(acceptance)


def api(path):
    request = urllib.request.Request("https://api.github.com/" + path,
                                     headers={"Authorization": "Bearer " + os.environ["GH_TOKEN"],
                                              "Accept": "application/vnd.github+json"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def pages(path, field=None):
    result = []
    for number in range(1, 101):
        data = api(path + ("&" if "?" in path else "?") + f"per_page=100&page={number}")
        batch = data[field] if field else data
        result.extend(batch)
        if len(batch) < 100:
            return result
    raise RuntimeError("GitHub pagination exceeded its bound")


def fetch_head(pull, event_repository):
    repository = pull["head"]["repo"]["full_name"]
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise RuntimeError("Invalid head repository")
    head = pull["head"]["sha"]
    if not re.fullmatch(r"[a-f0-9]{40}", head):
        raise RuntimeError("Invalid head SHA")
    pull_ref = f"refs/pull/{pull['number']}/head"
    note_refs = (acceptance.NOTE_REF, acceptance.PLAN_NOTE_REF)
    if repository == event_repository:
        specs = [f"+{note_ref}:{note_ref}" for note_ref in note_refs]
        combined = acceptance.git("fetch", "--atomic", "--no-tags", "origin", pull_ref, *specs, check=False)
        if combined.returncode == 0:
            return head
        error = combined.stderr.decode(errors="replace")
        # A repository can lack either optional namespace. The atomic failure
        # leaves all refs untouched for the original per-ref fetches below.
        missing = {f"fatal: couldn't find remote ref {note_ref}" for note_ref in note_refs}
        if missing.isdisjoint(line.strip() for line in error.splitlines()):
            raise RuntimeError(error.strip())
    acceptance.git("fetch", "--no-tags", "origin", pull_ref)
    for note_ref in note_refs:
        fetched = acceptance.git("fetch", "--no-tags", f"https://github.com/{repository}.git",
                                 f"+{note_ref}:{note_ref}", check=False)
        if fetched.returncode:
            diagnostic = fetched.stderr.decode(errors="replace")
            if f"fatal: couldn't find remote ref {note_ref}" not in diagnostic.splitlines():
                raise RuntimeError(diagnostic)
            # A merge group can include several forks. Keep each note lookup
            # scoped to the source remote even when that namespace is absent.
            acceptance.git("update-ref", "-d", note_ref)
    return head


def github_event():
    """Decode the UTF-8 event file supplied by GitHub Actions."""
    return json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text(encoding="utf-8"))


def verify_publisher(repository, identifier, status, head=None, require_bound=False):
    """Bind a rerun's status to its completed current jobs despite stale aggregates."""
    route = f"repos/{repository}/actions/runs/{identifier}"
    run = api(route)
    if (run["path"].split("@")[0] != ".github/workflows/local-acceptance-publish.yml"
            or run["event"] not in {"workflow_run", "workflow_dispatch"}):
        raise RuntimeError("Acceptance status was not produced by the trusted publisher")
    if run["event"] == "workflow_dispatch" and run.get("head_branch") != "main":
        raise RuntimeError("Manual acceptance finalization must execute the protected main workflow")
    title = run.get("display_title", "")
    if require_bound or run["event"] == "workflow_dispatch" or title.startswith("Local acceptance publish "):
        if head is None or title != "Local acceptance publish " + head:
            raise RuntimeError("Acceptance publisher is bound to another source head")
    attempt = run.get("run_attempt", 1)
    if type(attempt) is not int or attempt < 1:
        raise RuntimeError("Invalid acceptance publisher attempt")
    if attempt == 1:
        if run["conclusion"] not in (None, "success"):
            raise RuntimeError("Acceptance status was not produced by the trusted publisher")
        return
    # GitHub can retain the first failure in run/attempt aggregates after a
    # successful rerun. Its latest-jobs response can also include older jobs.
    jobs = pages(route + "/jobs?filter=latest", "jobs")
    current = [job for job in jobs if job.get("run_attempt") == attempt]
    if (any(type(job.get("run_attempt")) is not int or job["run_attempt"] > attempt for job in jobs)
            or not current or any(job.get("run_id") != int(identifier)
                or job.get("head_sha") != run["head_sha"]
                or job.get("status") != "completed" or job.get("conclusion") != "success"
                for job in current)):
        raise RuntimeError(f"Acceptance publisher attempt {attempt} has no complete successful current jobs. "
                           f"Inspect gh run view {identifier}; wait for completion, repair actual failures, "
                           "then enqueue the PR again with gh pr merge <PR> --merge --auto.")
    try:
        created = datetime.fromisoformat(status["created_at"].replace("Z", "+00:00"))
        windows = [(datetime.fromisoformat(job["started_at"].replace("Z", "+00:00")),
                    datetime.fromisoformat(job["completed_at"].replace("Z", "+00:00"))) for job in current]
        belongs = created.tzinfo is not None and any(start <= created <= end for start, end in windows)
    except (KeyError, TypeError, ValueError):
        belongs = False
    if not belongs:
        raise RuntimeError(f"Acceptance status does not belong to publisher attempt {attempt}. "
                           f"Inspect gh run view {identifier}; a fresh successful publication is required.")
    print(f"Verified publisher attempt {attempt} from {len(current)} completed successful jobs")


def signed_acceptance(head, policy_ref, execution_ref):
    """Schedule complementary hosts from a verified receipt or an enrolled author's launch."""
    author_complete = True
    launch = (acceptance.read_execution_plan(head, policy_ref, policy_ref, signers_ref=policy_ref)
              if acceptance.read_note(head, acceptance.PLAN_NOTE_REF)["attestations"] else None)
    try:
        receipt = acceptance.verify(head, policy_ref, execution_ref, signers_ref=policy_ref)
    except RuntimeError as missing_completion:
        # Scheduling uses a distinct SSH purpose and cannot produce the
        # required Local acceptance success. Merge groups never take this path.
        if launch is None:
            raise
        receipt = launch
        author_complete = False
        print("LOCAL_ACCEPTANCE_PENDING_AUTHOR: signed execution plan verified; start missing native hosts "
              "while the author runs all required local checks. Full signed acceptance remains required "
              "before the GitHub Actions Local acceptance status can succeed. "
              f"Current completion diagnostic: {missing_completion}", flush=True)
    if launch is None and "execution_plan" in receipt:
        launch = acceptance.verify_execution_plan(receipt["execution_plan"], head, policy_ref, policy_ref,
                                                   signers_ref=policy_ref)
    if launch is not None:
        if launch["plan"] != receipt["plan"] or launch["base"] != receipt["base"]:
            raise RuntimeError("Early native execution plan differs from the final author responsibility plan; rerun acceptance")
        # The independently verified note binds this immutable executor.
        # Current responsibilities, commands and native workflow/dispatcher
        # must still match; unrelated protected metadata can advance.
        execution_ref = launch["policy"]
    policy = acceptance.load_policy(execution_ref)
    current = acceptance.selected(policy, acceptance.changed_paths(receipt["base"], head), base=receipt["base"], head=head)
    plan = acceptance.acceptance_plan(policy, current, receipt["host"])
    return plan, receipt["base"], execution_ref, author_complete


def hosted_acceptance(head, base_ref, policy_ref):
    """Assign the original author responsibilities to a disposable Linux x64 worker."""
    base = acceptance.value("merge-base", base_ref, head)
    policy = acceptance.load_policy(policy_ref)
    names = acceptance.selected(policy, acceptance.changed_paths(base, head), base=base, head=head)
    if not names:
        raise RuntimeError("Hosted acceptance selected no checks")
    runner = acceptance.NATIVE_TARGETS["linux-x64"]["runner"]
    plan = acceptance.acceptance_plan(policy, names, {"system": "linux", "architecture": "x86_64"})
    identity = acceptance.sha(acceptance.canonical([head, base, acceptance.contract_hash(policy_ref), plan]))
    groups = {}
    for name in plan["local_checks"]:
        family = "sdk" if name == "native-sdk" else "native" if name.startswith("native-") else "offline"
        groups.setdefault(family, []).append(name)
    jobs = list(plan["ci"])
    for family, checks in groups.items():
        targets = list(dict.fromkeys(target for entry in plan["local_targets"] if entry["check"] in checks
                                     for target in entry["targets"]))
        jobs.append({"os": runner, "family": family, "checks": checks, "targets": targets,
                     "name": f"Hosted acceptance ({family}, {identity})",
                     "node": "20" if family == "sdk" else "22", "timeout": 180})
    return {"ci": jobs}, base


def required_jobs(plan):
    jobs = plan["ci"] if plan else []
    return ["Verify local acceptance", *[job["name"] for job in jobs]]


def verify_jobs(repository, run, names, execution_steps=None):
    jobs = pages(f"repos/{repository}/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs", "jobs")
    for name in names:
        matching = [job for job in jobs if job["name"] == name]
        if len(matching) != 1 or matching[0]["conclusion"] != "success":
            raise RuntimeError(f"Required current-attempt job did not pass: {name}")
        if execution_steps and name in execution_steps:
            steps = [step for step in matching[0]["steps"] if step["name"] == execution_steps[name]]
            if len(steps) != 1 or steps[0]["conclusion"] != "success":
                raise RuntimeError(f"Required offline execution step did not pass: {name}")


def unsigned_source(head):
    return not (acceptance.read_note(head)["attestations"]
                or acceptance.read_note(head, acceptance.PLAN_NOTE_REF)["attestations"])


def verify_hosted_run(repository, pull, source, plan):
    head = pull["head"]
    if (source["path"].split("@")[0] != ".github/workflows/local-acceptance.yml"
            or source["event"] != "pull_request" or source["head_sha"] != head["sha"]
            or source["head_repository"]["full_name"] != head["repo"]["full_name"]
            or source["status"] != "completed" or source["conclusion"] != "success"):
        raise RuntimeError("Hosted acceptance requires a successful run of this PR's exact source")
    steps = {job["name"]: ("Run the author's selected offline profiles on a hosted worker" if job["family"] == "offline"
                           else "Run the same native commands on the missing platform") for job in plan["ci"]}
    verify_jobs(repository, source, required_jobs(plan), steps)


def main():
    event = github_event()
    repository = os.environ["GITHUB_REPOSITORY"]
    policy_ref = acceptance.value("rev-parse", "HEAD")
    execution_ref = policy_ref
    verified_base = None
    plan = None
    author_complete = True
    hosted = False
    if "pull_request" in event:
        pull = api(f"repos/{repository}/pulls/{event['pull_request']['number']}")
        if pull["head"]["sha"] != event["pull_request"]["head"]["sha"]:
            raise RuntimeError("PR advanced while verification was starting")
        head = fetch_head(pull, repository)
        # The API/event base SHA can lag main even on a new ready event. The
        # checked-out protected commit owns both trust and the comparison base.
        execution_ref = acceptance.authorized_policy(head, policy_ref, policy_ref, api)
        if unsigned_source(head) and execution_ref == policy_ref:
            hosted = True
            plan, verified_base = hosted_acceptance(head, policy_ref, policy_ref)
            print(f"Hosted offline acceptance for {head}: {json.dumps(plan)}", flush=True)
        else:
            plan, verified_base, execution_ref, author_complete = signed_acceptance(head, policy_ref, execution_ref)
    else:
        group = event["merge_group"]
        acceptance.git("fetch", "--no-tags", "origin", group["head_sha"], group["base_sha"])
        commits = set(acceptance.value("rev-list", f"{group['base_sha']}..{group['head_sha']}").splitlines())
        pulls = [pull for pull in pages(f"repos/{repository}/pulls?state=open&base=main")
                 if pull["head"]["sha"] in commits]
        if not pulls:
            raise RuntimeError("Cannot attribute this merge group to current PR heads")
        for pull in pulls:
            head = fetch_head(pull, repository)
            authorized = acceptance.authorized_policy(head, group["base_sha"], policy_ref, api)
            statuses = pages(f"repos/{repository}/commits/{head}/statuses")
            statuses = [status for status in statuses if status["context"] == "Local acceptance"]
            status = statuses[0] if statuses else None
            prefix = f"https://github.com/{repository}/actions/runs/"
            if not status or status["state"] != "success" or status["creator"]["login"] != "github-actions[bot]" or not (status.get("target_url") or "").startswith(prefix):
                raise RuntimeError(f"PR #{pull['number']} has no completed cross-platform acceptance for {head}")
            identifier = status["target_url"][len(prefix):]
            if not identifier.isdecimal():
                raise RuntimeError("Invalid acceptance publisher run URL")
            hosted_run = re.match(r"^Hosted offline run ([1-9][0-9]*) passed\.", status.get("description") or "")
            if hosted_run and authorized == policy_ref:
                verify_publisher(repository, identifier, status, head=head, require_bound=True)
                hosted_plan, _ = hosted_acceptance(head, group["base_sha"], policy_ref)
                source = api(f"repos/{repository}/actions/runs/{hosted_run[1]}")
                verify_hosted_run(repository, pull, source, hosted_plan)
            else:
                receipt = acceptance.verify(head, group["base_sha"], authorized, signers_ref=policy_ref)
                verify_publisher(repository, identifier, status, head=head,
                    require_bound=bool(receipt.get("execution_plan")
                                       or acceptance.read_note(head, acceptance.PLAN_NOTE_REF)["attestations"]))
            print(f"Carried PR #{pull['number']} {head[:12]} from publisher run {identifier}")
        subprocess.run([sys.executable, str(ROOT / ".github/scripts/check_pr_history.py"),
                        group["base_sha"], group["head_sha"]], cwd=ROOT, check=True)
    jobs = plan["ci"] if plan else []
    # Existing PR workflows and the protected publisher still consume the old
    # native-only fields during rollout. New workflows use the complete matrix.
    native = [job for job in jobs if job["family"] == "native"]
    checks = list(dict.fromkeys(name for job in native for name in job["checks"]))
    platforms = list(dict.fromkeys(job["os"] for job in native))
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        stream.write("needed=" + ("true" if jobs else "false") + "\n")
        # A skipped job still needs a structurally valid Actions matrix.
        stream.write("matrix=" + json.dumps({"include": jobs or [{"os": "ubuntu-latest", "name": "No missing hosts",
                     "family": "native", "node": "22", "timeout": 90, "checks": []}]}) + "\n")
        stream.write("policy_ref=" + execution_ref + "\n")
        stream.write("base_sha=" + (verified_base or "") + "\n")
        stream.write("checks=" + " ".join(checks) + "\n")
        stream.write("platforms=" + json.dumps(platforms or ["ubuntu-latest"]) + "\n")
    return {"plan": plan, "checks": checks, "platforms": platforms,
            "author_complete": author_complete,
            "hosted": hosted, "required_jobs": required_jobs(plan)}


if __name__ == "__main__":
    main()
