#!/usr/bin/env python3
"""Exercise current-main and preserved-history checks against actual Git repos."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile

GATE = Path(__file__).resolve().parents[2] / ".github/scripts/check_pr_history.py"


def main():
    with tempfile.TemporaryDirectory(prefix="pr-history-") as directory:
        root = Path(directory)
        env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1", "GIT_EDITOR": "true"}

        def git(*args, check=True):
            return subprocess.run(["git", *args], cwd=root, env=env, check=check, capture_output=True, text=True)

        def commit(name, content):
            (root / name).write_text(content)
            git("add", name)
            git("commit", "-m", "Change " + name)

        def expect(label, success, error=None):
            result = subprocess.run([sys.executable, str(GATE), "main", "HEAD"], cwd=root, env=env, capture_output=True, text=True)
            if (result.returncode == 0) != success:
                raise AssertionError(label + "\n" + result.stdout + result.stderr)
            if error is not None and error not in result.stderr:
                raise AssertionError(label + " did not fail for the expected reason\n" + result.stdout + result.stderr)
            print("PASS: " + label)

        git("init", "-b", "main")
        git("config", "user.name", "Gate acceptance")
        git("config", "user.email", "gate@example.invalid")
        commit("shared.txt", "original\n")
        git("switch", "-c", "previous-feature")
        commit("previous.txt", "already merged\n")
        git("switch", "main")
        git("merge", "--no-ff", "previous-feature", "-m", "Merge previous feature")
        git("switch", "-c", "feature")
        commit("shared.txt", "feature change\n")
        expect("existing main merge history accepted", True)
        feature_commit = git("rev-parse", "HEAD").stdout.strip()
        feature_author = git("show", "-s", "--format=%an <%ae>", "HEAD").stdout.strip()

        git("switch", "main")
        commit("shared.txt", "main change\n")
        git("switch", "feature")
        expect("stale PR rejected", False, "PR is behind main")
        main_commit = git("rev-parse", "main").stdout.strip()
        assert git("merge", "main", "-m", "Merge main", check=False).returncode != 0
        (root / "shared.txt").write_text("feature change\nmain change\n")
        git("add", "shared.txt")
        git("commit", "-m", "Resolve current main")
        expect("resolved merge with current main accepted", True)
        git("merge-base", "--is-ancestor", feature_commit, "HEAD")
        git("merge-base", "--is-ancestor", main_commit, "HEAD")
        assert git("show", "-s", "--format=%an <%ae>", feature_commit).stdout.strip() == feature_author
        print("PASS: original feature commit, author and both parent histories remain intact")

        for index, marker in enumerate(("<<<<<<< HEAD\n", "||||||| base\n", "=======\n", ">>>>>>> branch\n")):
            git("switch", "-c", f"marker-{index}", "feature")
            commit("conflict.txt", marker)
            expect("leftover marker rejected: " + marker.strip(), False, "Resolve the leftover conflict markers")
        git("switch", "feature")
        commit("notes.md", "Markdown hard break  \nnext line\n")
        expect("intentional Markdown whitespace accepted", True)

        git("switch", "main")
        commit("next-main.txt", "main advances again\n")
        git("switch", "feature")
        expect("new main changes require integration", False, "PR is behind main")
        git("merge", "main", "-m", "Merge main again")
        expect("another main integration accepted", True)
        git("merge-base", "--is-ancestor", feature_commit, "HEAD")

        git("switch", "-c", "additional-feature", "main")
        commit("additional.txt", "another change\n")
        additional_commit = git("rev-parse", "HEAD").stdout.strip()
        git("switch", "feature")
        git("merge", "--no-ff", "additional-feature", "-m", "Merge additional feature")
        expect("additional preserved branch history accepted", True)
        git("merge-base", "--is-ancestor", additional_commit, "HEAD")


if __name__ == "__main__":
    main()
