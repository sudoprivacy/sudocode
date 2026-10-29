#!/usr/bin/env python3
"""Select expensive provider/daemon acceptance from actual changed Git inputs."""

import fnmatch
import json
import os
from pathlib import Path
import re
import subprocess


def git(*args):
    return subprocess.check_output(["git", *args]).decode().strip()


def main():
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text(encoding="utf-8"))
    name = os.environ["GITHUB_EVENT_NAME"]
    if name == "workflow_dispatch":
        paths, live, daemon = [], True, True
    else:
        if name == "push":
            base, head = event["before"], event["after"]
        elif name == "pull_request":
            base, head = event["pull_request"]["base"]["sha"], event["pull_request"]["head"]["sha"]
        elif name == "merge_group":
            base, head = event["merge_group"]["base_sha"], event["merge_group"]["head_sha"]
        else:
            raise RuntimeError(f"Unsupported CI event: {name}")
        if not all(re.fullmatch(r"[a-f0-9]{40}", ref) for ref in [base, head]):
            raise RuntimeError("Invalid CI commit identity")
        if base == "0" * 40:
            paths, live, daemon = [], True, True
        else:
            for ref in [base, head]:
                if subprocess.run(["git", "cat-file", "-e", ref + "^{commit}"], capture_output=True).returncode:
                    subprocess.run(["git", "fetch", "--no-tags", "origin", ref], check=True)
            if name == "pull_request":
                base = git("merge-base", base, head)
            paths = git("diff", "--name-only", "--no-renames", base, head).splitlines()
            relevant = [path for path in paths if not path.lower().endswith((".md", ".html"))
                        or (path.startswith("rust/") and Path(path).name.lower() != "readme.md")]
            live_inputs = ["rust/**", "spec/**", "e2e/thinking/**", ".github/scripts/probe_live_usage*",
                           ".github/scripts/live_model_evidence.cjs", ".github/scripts/model_compat.py"]
            daemon_inputs = ["rust/**", "e2e/nexus-a2a/**", "e2e/shared-services/**", ".github/ci/**"]
            live = any(fnmatch.fnmatchcase(path, rule) for path in relevant for rule in live_inputs)
            daemon = any(fnmatch.fnmatchcase(path, rule) for path in relevant for rule in daemon_inputs)
    result = {"live_required": str(live).lower(), "daemon_required": str(daemon).lower()}
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        for key, value in result.items():
            stream.write(f"{key}={value}\n")
    print(json.dumps({**result, "changed_paths": paths}))


if __name__ == "__main__":
    main()
