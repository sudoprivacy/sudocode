#!/usr/bin/env python3
"""Require current main, preserve base integrations, and reject conflict markers."""

import subprocess
import sys


def is_ancestor(ancestor, descendant):
    return subprocess.run(["git", "merge-base", "--is-ancestor", ancestor, descendant]).returncode == 0


def main():
    base, head = sys.argv[1:]
    base = subprocess.check_output(
        ["git", "rev-parse", "--verify", base + "^{commit}"], text=True
    ).strip()
    head = subprocess.check_output(
        ["git", "rev-parse", "--verify", head + "^{commit}"], text=True
    ).strip()
    if not is_ancestor(base, head):
        sys.exit("PR is behind main. Fetch origin, merge origin/main, resolve conflicts, and push.")
    merges = subprocess.check_output(
        ["git", "rev-list", "--parents", "--min-parents=2", f"{base}..{head}"], text=True
    ).strip()
    for merge in merges.splitlines():
        parents = merge.split()
        if len(parents) != 3 or not is_ancestor(parents[2], base):
            sys.exit("PR merges a branch outside main's history. Only integrations of main are allowed:\n" + merge)
    # Check Git's conflict markers without imposing new whitespace rules on docs.
    result = subprocess.run([
        "git", "-c", "core.whitespace=-blank-at-eol,-blank-at-eof,-space-before-tab",
        "diff", "--check", base, head,
    ])
    if result.returncode:
        sys.exit("Resolve the leftover conflict markers before merging.")
    print("PR includes main, only integrates main's history, and has no new conflict markers.")


if __name__ == "__main__":
    main()
