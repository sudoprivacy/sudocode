"""Trusted, read-only metadata gate; candidate sources are never executed here."""

import importlib.util
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


def fetch_head(pull):
    repository = pull["head"]["repo"]["full_name"]
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise RuntimeError("Invalid head repository")
    head = pull["head"]["sha"]
    if not re.fullmatch(r"[a-f0-9]{40}", head):
        raise RuntimeError("Invalid head SHA")
    acceptance.git("fetch", "--no-tags", "origin", f"refs/pull/{pull['number']}/head")
    acceptance.git("fetch", "--no-tags", f"https://github.com/{repository}.git",
                   f"+{acceptance.NOTE_REF}:{acceptance.NOTE_REF}")
    return head


def main():
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    repository = os.environ["GITHUB_REPOSITORY"]
    policy_ref = acceptance.value("rev-parse", "HEAD")
    native = []
    operating_systems = []
    if "pull_request" in event:
        pull = api(f"repos/{repository}/pulls/{event['pull_request']['number']}")
        if pull["head"]["sha"] != event["pull_request"]["head"]["sha"]:
            raise RuntimeError("PR advanced while verification was starting")
        head = fetch_head(pull)
        receipt = acceptance.verify(head, pull["base"]["sha"], policy_ref)
        current = acceptance.selected(acceptance.load_policy(policy_ref), acceptance.changed_paths(receipt["base"], head))
        native = [name for name in current if name.startswith("native-")]
        # The local host supplies its own platform; CI supplies the other one.
        # Keep one Linux workspace run as well. The queue carries this exact
        # head's completed workflow instead of rebuilding all three again.
        operating_systems = {"windows": ["ubuntu-latest", "macos-latest"],
                             "macos": ["ubuntu-latest", "windows-latest"],
                             "linux": ["windows-latest", "macos-latest"]}[receipt["host"]["system"]]
    else:
        group = event["merge_group"]
        acceptance.git("fetch", "--no-tags", "origin", group["head_sha"], group["base_sha"])
        commits = set(acceptance.value("rev-list", f"{group['base_sha']}..{group['head_sha']}").splitlines())
        pulls = [pull for pull in pages(f"repos/{repository}/pulls?state=open&base=main")
                 if pull["head"]["sha"] in commits]
        if not pulls:
            raise RuntimeError("Cannot attribute this merge group to current PR heads")
        for pull in pulls:
            head = fetch_head(pull)
            acceptance.verify(head, group["base_sha"], policy_ref)
            statuses = pages(f"repos/{repository}/commits/{head}/statuses")
            statuses = [status for status in statuses if status["context"] == "Local acceptance"]
            status = statuses[0] if statuses else None
            prefix = f"https://github.com/{repository}/actions/runs/"
            if not status or status["state"] != "success" or status["creator"]["login"] != "github-actions[bot]" or not (status.get("target_url") or "").startswith(prefix):
                raise RuntimeError(f"PR #{pull['number']} has no completed cross-platform acceptance for {head}")
            identifier = status["target_url"][len(prefix):]
            if not identifier.isdecimal():
                raise RuntimeError("Invalid acceptance publisher run URL")
            run = api(f"repos/{repository}/actions/runs/{identifier}")
            if run["path"].split("@")[0] != ".github/workflows/local-acceptance-publish.yml" or run["event"] != "workflow_run" or run["conclusion"] not in (None, "success"):
                raise RuntimeError("Acceptance status was not produced by the trusted publisher")
            print(f"Carried PR #{pull['number']} {head[:12]} from publisher run {identifier}")
        subprocess.run([sys.executable, str(ROOT / ".github/scripts/check_pr_history.py"),
                        group["base_sha"], group["head_sha"]], cwd=ROOT, check=True)
    with open(os.environ["GITHUB_OUTPUT"], "a") as stream:
        stream.write("needed=" + ("true" if native else "false") + "\n")
        stream.write("checks=" + " ".join(native) + "\n")
        stream.write("platforms=" + json.dumps(operating_systems or ["ubuntu-latest"]) + "\n")
    return {"checks": native, "platforms": operating_systems if native else []}


if __name__ == "__main__":
    main()
