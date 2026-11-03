# AGENTS.md

Read [`CONTRIBUTING.md`](./CONTRIBUTING.md) — that is the canonical
guide for working in this repository. Human contributors and AI
agents follow the same rules.

The previous content of this file (working agreement, parity
standing rule, verification commands, ShareOne publishing recipe,
documentation map) has been migrated into `CONTRIBUTING.md` so the
rules apply to every contributor by default, not just to Claude.
This file remains as a thin pointer because agents read it on
startup by convention; the pointer ensures the agent finds the
canonical doc.

## Landing a PR

`main` lands through a merge queue. Land with `gh pr merge <n> --merge --auto`; direct and `--admin` merges are refused by design. Don't update a branch just to catch up with `main`. Open PRs as drafts while iterating and mark them ready once: heavy CI runs only on ready PRs and in the queue. If landing looks blocked, never delete, disable or edit the merge-queue ruleset or the branch protection: they guarantee every commit on `main` passed CI. Tell the user instead. Details: [Pull request guidelines](./CONTRIBUTING.md#pull-request-guidelines).
