#!/usr/bin/env python3
"""Reject merge commits that carry edits Git's merge would not have made, and
leftover conflict markers.

`check_pr_history.py <base> <head>` checks every merge commit in
`merge-base(base, head)..head`. For a pull request, base is `origin/main`; in
a merge queue, base is the group's base commit — which also checks the merges
GitHub itself made. A clean PR may be behind main: the queue integrates it.
Actual conflicts with the supplied target or fetched origin/main fail before
expensive local acceptance. The checkout and index remain untouched.

For each merge, Git's own merge of its two parents is recomputed
(`git merge-tree`). A file that differs from that result is allowed only if it
actually conflicted, because resolving a conflict is the one edit a merge must
make. Any other difference is an edit hidden inside a merge commit — the way
agents' merge-from-main resolutions have silently reverted other PRs' work.
This is the same rule as nexi-lab/nexus's `check_merge_history.py`.

An intended adaptation belongs in its own commit after the merge. A merge that
is already published cannot be rewritten, so it can instead be declared with a
`Merge-adapts: <path>` trailer on any commit in the range.
"""

import json
import subprocess
import sys


def git(*args):
    return subprocess.check_output(["git", *args])


def commit(ref):
    return git("rev-parse", "--verify", "--end-of-options", ref + "^{commit}").decode().strip()


def paths(output):
    return set(output.rstrip(b"\0").split(b"\0")) - {b""}


def merge_tree(left, right):
    result = subprocess.run(
        ["git", "merge-tree", "--write-tree", "--name-only", "--no-messages", "-z", left, right],
        capture_output=True,
    )
    if result.returncode not in (0, 1):
        sys.exit(f"Cannot recompute merge {left[:12]} + {right[:12]}: {result.stderr.decode(errors='replace')}")
    tree, _, conflicted = result.stdout.partition(b"\0")
    return tree.decode(), paths(conflicted)


def target_conflicts(base, head):
    targets = [("supplied base", base)]
    # Local acceptance passes the diff's merge-base. Its normal fetch has
    # already updated origin/main; check that tip too without doing network I/O.
    present = subprocess.run(["git", "show-ref", "--verify", "--quiet", "refs/remotes/origin/main"])
    if present.returncode == 0:
        target = commit("refs/remotes/origin/main")
        if target != base:
            targets.append(("origin/main", target))
    elif present.returncode != 1:
        sys.exit("Cannot inspect fetched origin/main for merge conflicts.")
    errors = []
    for label, target in targets:
        _, conflicted = merge_tree(target, head)
        if conflicted:
            errors.append({"target": label, "commit": target,
                           "paths": [path.decode(errors="replace") for path in sorted(conflicted)]})
    return errors


def hidden_merge_edits(base, head):
    declared = {
        line.strip().encode()
        for line in git(
            "log", "--format=%(trailers:key=Merge-adapts,valueonly,separator=%x0a)", f"{base}..{head}"
        ).decode().splitlines()
        if line.strip()
    }
    errors = []
    for merge in git("rev-list", "--merges", f"{base}..{head}").decode().split():
        parents = git("show", "-s", "--format=%P", merge).decode().split()
        if len(parents) != 2:
            errors.append(f"{merge[:12]}: only two-parent merges can be verified")
            continue
        tree, conflicted = merge_tree(*parents)
        edited = paths(git("diff", "--no-renames", "--name-only", "-z", tree, merge))
        for path in sorted(edited - conflicted - declared):
            errors.append(f"{merge[:12]}  {path.decode(errors='replace')}")
    return errors


def main():
    target, head = (commit(ref) for ref in sys.argv[1:])
    base = git("merge-base", target, head).decode().strip()
    errors = hidden_merge_edits(base, head)
    if errors:
        sys.exit(
            "These merge commits change files beyond what Git's merge produces, in files\n"
            "that did not conflict:\n"
            + "".join(f"  {error}\n" for error in errors)
            + "That is how other PRs' work gets silently reverted. If it was unintended, restore\n"
            "the merged result. If it was an intended adaptation, make it visible: put it in its\n"
            "own commit, or declare a published merge on any commit with a trailer, e.g.\n"
            "  git commit --allow-empty -m 'Declare merge adaptation' -m 'Merge-adapts: <path>'"
        )
    conflicts = target_conflicts(target, head)
    if conflicts:
        print("LOCAL_ACCEPTANCE_MERGE_CONFLICTS=" + json.dumps({"head": head, "conflicts": conflicts}),
              file=sys.stderr)
        sys.exit(
            "Resolve these actual target conflicts before expensive local acceptance:\n"
            + "".join(f"  {item['target']} ({item['commit'][:12]}): {path}\n"
                      for item in conflicts for path in item["paths"])
            + "Integrate the fetched target on your branch, preserve both sides, and commit the resolutions.\n"
            "Then rerun local acceptance for the resulting commit. Clean branches may remain behind main;\n"
            "there is no need to update a branch merely to catch up. No checkout or index files were changed."
        )
    # Check Git's conflict markers without imposing new whitespace rules on docs.
    result = subprocess.run([
        "git", "-c", "core.whitespace=-blank-at-eol,-blank-at-eof,-space-before-tab",
        "diff", "--check", base, head,
    ])
    if result.returncode:
        sys.exit("Resolve the leftover conflict markers before merging.")
    print("Every merge commit matches Git's merge outside its conflicts; no conflict markers or target conflicts.")


if __name__ == "__main__":
    main()
