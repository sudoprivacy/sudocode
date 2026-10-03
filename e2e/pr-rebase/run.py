#!/usr/bin/env python3
"""Run the merge gate against real repositories and a conflict-resolving rebase."""

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
        expect("rebased PR accepts existing main merge commits", True)

        git("switch", "main")
        commit("shared.txt", "main change\n")
        git("switch", "feature")
        expect("stale PR rejected", False)
        git("branch", "merge-update")
        result = git("rebase", "main", check=False)
        assert result.returncode != 0, "expected a real rebase conflict"
        (root / "shared.txt").write_text("resolved change\n")
        git("add", "shared.txt")
        git("rebase", "--continue")
        expect("resolved rebase accepted", True)

        for marker in ("<<<<<<< HEAD\n", "||||||| base\n", "=======\n", ">>>>>>> branch\n"):
            commit("conflict.txt", marker)
            expect("leftover marker rejected: " + marker.strip(), False)
            git("reset", "--hard", "HEAD~1")
        commit("notes.md", "Markdown hard break  \nnext line\n")
        expect("intentional Markdown whitespace accepted", True)

        git("switch", "merge-update")
        result = git("merge", "main", "-m", "Merge main", check=False)
        assert result.returncode != 0, "expected a real merge conflict"
        (root / "shared.txt").write_text("resolved merge\n")
        git("add", "shared.txt")
        git("commit", "-m", "Resolve merge")
        expect("merging main rejected even after resolving conflicts", False)


if __name__ == "__main__":
    main()
