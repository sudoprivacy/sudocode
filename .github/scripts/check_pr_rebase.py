#!/usr/bin/env python3
"""Reject stale PRs, merge commits in their range, and leftover conflict markers."""

import subprocess
import sys


def main():
    base, head = sys.argv[1:]
    base = subprocess.check_output(
        ["git", "rev-parse", "--verify", base + "^{commit}"], text=True
    ).strip()
    head = subprocess.check_output(
        ["git", "rev-parse", "--verify", head + "^{commit}"], text=True
    ).strip()
    if subprocess.run(["git", "merge-base", "--is-ancestor", base, head]).returncode:
        sys.exit("PR is behind main. Run git fetch origin && git rebase origin/main, then git push --force-with-lease.")
    merges = subprocess.check_output(
        ["git", "rev-list", "--min-parents=2", f"{base}..{head}"], text=True
    ).strip()
    if merges:
        sys.exit("PR contains merge commits. Rebase onto origin/main instead of merging main into the PR:\n" + merges)
    # Check Git's conflict markers without imposing new whitespace rules on docs.
    result = subprocess.run([
        "git", "-c", "core.whitespace=-blank-at-eol,-blank-at-eof,-space-before-tab",
        "diff", "--check", base, head,
    ])
    if result.returncode:
        sys.exit("Resolve the leftover conflict markers before merging.")
    print("PR includes main, has no merge commits of its own, and has no new conflict markers.")


if __name__ == "__main__":
    main()
