# Parity

This document describes how `scode` tracks feature parity with
`anthropics/claude-code`: the scope of "parity," the comparison sources
used, the resolution taxonomy applied to each gap, and the sync marker
that anchors the comparison window.

The roadmap that owns this work is [`../ROADMAP.md`](../ROADMAP.md)
(Goal 2).

## Scope

Parity covers four dimensions, ordered by visibility to a user of
`scode`:

1. **Tool surface** — the set of tools the agent can call and the
   shape of their request and response payloads.
2. **Slash commands** — the set of `/`-prefixed commands available at
   the REPL and their behaviors.
3. **Behavioral semantics** — runtime behaviors that surface to the
   user: output truncation, session compaction, token counting, cost
   tracking, retry strategy, permission enforcement.
4. **Configuration surface** — the file layout, key names, and
   precedence rules for `.scode.json` and related configuration
   sources.

## Reference target

The reference target is `anthropics/claude-code`. The public surfaces
used to derive the comparison are:

- The published CHANGELOG entries on `anthropics/claude-code`.
- The published `@anthropic-ai/claude-code` npm package, used to
  extract the live tool list, slash command list, and bundled prompt
  resources.
- The official documentation at `docs.claude.com`.

## Optional cherry-pick source

`ultraworkers/claw-code` is a Rust port of the same family. Where its
implementation of an overlapping feature fits `scode`'s shape, the
commit can be cherry-picked into our tree. The relationship is
optional: `claw-code` is treated as a candidate source for Rust-side
implementations, separate from the reference target above.

## Resolution taxonomy

Every parity gap carries a one-word tag and a short rationale.

| Tag | Meaning |
|---|---|
| `[BUILD]` | The gap will be closed by implementing the feature in `scode`. |
| `[CHERRY-PICK]` | The gap will be closed by lifting a `claw-code` commit. |
| `[SKIP]` | The gap is closed by intent: `scode` provides the capability differently, the feature is replaced upstream, or it does not apply. |
| `[N/A]` | The gap belongs to a layer `scode` does not implement (for example, npm packaging). |
| `[OBSERVE]` | The gap is left open while waiting on more signal — typically how `claw-code` ports the feature, or whether real users hit it. |

## Sync marker

`LAST_PARITY_SYNC_COMMIT` at the repo root records the upstream
reference SHA the parity table was last anchored to. The sync marker is
mechanical state used by tooling; the gap inventory and resolutions
live alongside their owning issues, PRs, and design notes.

## Comparison window

The comparison window for the current cycle starts at
`anthropics/claude-code` releases shipped on or after **2026-01-01**.

## Measurement

The e2e coverage half of parity (Goal 1 in
[`../ROADMAP.md`](../ROADMAP.md)) is exercised by the mock parity
harness described in [`mock-parity-harness.md`](./mock-parity-harness.md).
The harness runs against the `scode` binary in a clean environment and
covers the scode-native testable feature surface.
