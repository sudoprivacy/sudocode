#!/usr/bin/env python3
"""Exercise the PR history gate with real merges and resolved conflicts."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile

GATE = Path(__file__).resolve().parents[2] / ".github/scripts/check_pr_rebase.py"


def main():
    with tempfile.TemporaryDirectory(prefix="pr-rebase-") as directory:
        root = Path(directory)
        env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1", "GIT_EDITOR": "true"}

        def git(*args, check=True):
            return subprocess.run(["git", *args], cwd=root, env=env, check=check, capture_output=True, text=True)

        def commit(name, content):
            (root / name).write_text(content)
            git("add", name)
            git("commit", "-m", "Change " + name)

        def expect(label, success):
            result = subprocess.run([sys.executable, str(GATE), "main", "HEAD"], cwd=root, env=env, capture_output=True, text=True)
            if (result.returncode == 0) != success:
                raise AssertionError(label + "\n" + result.stdout + result.stderr)
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
        expect("PR accepts existing main merge commits", True)
        feature_commit = git("rev-parse", "HEAD").stdout.strip()

        git("switch", "main")
        commit("shared.txt", "main change\n")
        git("switch", "feature")
        expect("stale PR rejected", False)
        main_commit = git("rev-parse", "main").stdout.strip()
        result = git("merge", "main", "-m", "Merge current main", check=False)
        assert result.returncode != 0, "expected a real merge conflict"
        (root / "shared.txt").write_text("resolved merge\n")
        git("add", "shared.txt")
        git("commit", "-m", "Resolve merge with main")
        expect("resolved merge with main accepted", True)
        git("merge-base", "--is-ancestor", feature_commit, "HEAD")
        git("merge-base", "--is-ancestor", main_commit, "HEAD")
        print("PASS: both original histories remain reachable")

        for marker in ("<<<<<<< HEAD\n", "||||||| base\n", "=======\n", ">>>>>>> branch\n"):
            commit("conflict.txt", marker)
            expect("leftover marker rejected: " + marker.strip(), False)
            git("reset", "--hard", "HEAD~1")
        commit("notes.md", "Markdown hard break  \nnext line\n")
        expect("intentional Markdown whitespace accepted", True)

        git("switch", "main")
        commit("next-main.txt", "main advances again\n")
        git("switch", "feature")
        expect("PR becomes stale when main advances again", False)
        git("merge", "main", "-m", "Merge main again")
        expect("multiple main integrations preserve history", True)

        git("switch", "-c", "unrelated", "main")
        commit("unrelated.txt", "another feature\n")
        git("switch", "feature")
        git("merge", "--no-ff", "unrelated", "-m", "Merge unrelated feature")
        expect("unrelated branch merge rejected", False)

        git("switch", "-c", "octopus", "main")
        commit("octopus.txt", "feature\n")
        git("switch", "-c", "second-unrelated", "main")
        commit("second-unrelated.txt", "another feature\n")
        git("switch", "octopus")
        git("merge", "unrelated", "second-unrelated", "-m", "Merge multiple branches")
        expect("octopus merge rejected", False)


if __name__ == "__main__":
    main()
