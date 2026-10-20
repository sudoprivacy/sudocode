#!/usr/bin/env python3
"""Require current main ancestry and reject unresolved conflict markers."""

import subprocess
import sys


def is_ancestor(ancestor, descendant):
    return subprocess.run(["git", "merge-base", "--is-ancestor", ancestor, descendant]).returncode == 0


def main():
    base, head = sys.argv[1:]
    base = subprocess.check_output(
        ["git", "rev-parse", "--verify", "--end-of-options", base + "^{commit}"], text=True
    ).strip()
    head = subprocess.check_output(
        ["git", "rev-parse", "--verify", "--end-of-options", head + "^{commit}"], text=True
    ).strip()
    if not is_ancestor(base, head):
        sys.exit("PR is behind main. Fetch origin and merge origin/main, resolve conflicts, and push while preserving history.")
    # Check Git's conflict markers without imposing new whitespace rules on docs.
    result = subprocess.run([
        "git", "-c", "core.whitespace=-blank-at-eol,-blank-at-eof,-space-before-tab",
        "diff", "--check", base, head,
    ])
    if result.returncode:
        sys.exit("Resolve the leftover conflict markers before merging.")
    print("PR contains current main and has no new conflict markers; commit history is preserved.")


if __name__ == "__main__":
    main()
