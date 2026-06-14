# Parity

This document describes how `scode` tracks feature parity with
`anthropics/claude-code`: the scope of "parity," the three reference
sources we compare against, the resolution taxonomy applied to each gap,
and the sync markers that anchor each source to a point in time.

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

## Three reference sources

We work against three sources, each with a different role. Treat them
in this order — start with the source-of-truth signal, then verify
against the high-fidelity reference, then look for a Rust
implementation we can lift.

### Tier 1 — source of truth: `anthropics/claude-code`

The reference target. The public surfaces used to derive what we are
tracking against are:

- The published CHANGELOG entries on `anthropics/claude-code`.
- The published `@anthropic-ai/claude-code` npm package, used to
  extract the live tool list, slash command list, and bundled prompt
  resources.
- The official documentation at `docs.claude.com`.

The source itself is private, so this tier gives feature-level signal,
not commit-level.

### Tier 2 — behavioral reference: `claude-code-best/claude-code`

`claude-code-best/claude-code` (`CCB`) is a TypeScript reconstruction
of Claude Code that aims for high source-level fidelity. It is
**treated as our running approximation of what CC actually does** when
the CHANGELOG signal alone is too coarse.

**Standing assumption.** CCB is sufficiently close to upstream
`anthropics/claude-code` that reading its source informs our
understanding of CC's behavior. The assumption is approximate, not
absolute — when CCB and CHANGELOG diverge, CHANGELOG wins.

**Mandatory workflow.** Whenever we make a parity decision based on a
CHANGELOG entry, we **also** open CCB and check the relevant code path.
This applies to every entry, including the ones that look obvious — the
nuance is usually in the path we did not anticipate.

```
CC CHANGELOG entry
        │
        ▼
  grep CCB source for the feature
        │
        ▼
  align understanding ── any surprise? → revisit the resolution
        │
        ▼
  pick resolution tag (see "Resolution taxonomy" below)
```

**What to grep.** CCB is monorepo-shaped TypeScript:

- `src/` — agent loop, REPL, tool dispatch, slash commands, system prompt.
- `packages/` — modular components (tool implementations, MCP, hooks).
- `docs/` — Mintlify-based feature documentation.

For a CHANGELOG entry like *"added `/cd` command to move a session to a
new working directory,"* searches that work:

```bash
# In a local CCB clone:
grep -rn -E "/cd\\b|case 'cd'|slash.*cd" src/ packages/
grep -rn "working directory" docs/ src/
```

**What CCB is not.** CCB is a reconstruction shipped as TypeScript and
distributed under "for learning/research only" terms. **It is not a
cherry-pick source for our Rust tree.** We read it for understanding;
we do not lift TypeScript into Rust. Citations in commit messages are
welcome ("matches the behavior in CCB `src/...`"), but the code itself
is re-implemented in Rust from understanding.

**When CCB does not help.** If CCB itself does not implement the
feature (it lags upstream by some window), record that fact in the
resolution: the gap stays anchored to the CC CHANGELOG entry and the
resolution carries an `[OBSERVE]` tag pending more signal.

### Tier 3 — optional cherry-pick source: `ultraworkers/claw-code`

`ultraworkers/claw-code` is a Rust port of the same family. Where its
implementation of an overlapping feature fits `scode`'s shape, the
commit can be cherry-picked into our tree. The relationship is
optional: `claw-code` is treated as a candidate source for Rust-side
implementations, separate from the reference target above.

Methodology for cherry-pick triage lives in
`scripts/triage_claw_code.py` and the cycle's report in
`docs/parity-claw-code-sync-YYYY-WW.md`.

## Resolution taxonomy

Every parity gap carries a one-word tag and a short rationale.

| Tag | Meaning |
|---|---|
| `[BUILD]` | The gap will be closed by implementing the feature in `scode`. |
| `[CHERRY-PICK]` | The gap will be closed by lifting a `claw-code` commit. |
| `[SKIP]` | The gap is closed by intent: `scode` provides the capability differently, the feature is replaced upstream, or it does not apply. |
| `[N/A]` | The gap belongs to a layer `scode` does not implement (for example, npm packaging). |
| `[OBSERVE]` | The gap is left open while waiting on more signal — typically how `claw-code` ports the feature, whether CCB has implemented it yet, or whether real users hit it. |

## Sync markers

Two marker files at the repo root anchor each source to a point in
time:

| File | Source | What it records |
|---|---|---|
| `LAST_PARITY_SYNC_COMMIT` | `ultraworkers/claw-code` HEAD | The claw-code SHA the last cherry-pick cycle was triaged against. |
| `LAST_CCB_REF_VERSION` | `claude-code-best/claude-code` HEAD | The CCB SHA our behavioral references currently point at. Bumped when we re-clone CCB for a new cycle. |

Sync markers are mechanical state used by tooling and triage scripts;
the gap inventory and per-feature resolutions live alongside their
owning issues, PRs, and design notes.

## Comparison window

The comparison window for the current cycle starts at
`anthropics/claude-code` releases shipped on or after **2026-01-01**.

## Measurement

The e2e coverage half of parity (Goal 1 in
[`../ROADMAP.md`](../ROADMAP.md)) is exercised by the mock parity
harness described in [`mock-parity-harness.md`](./mock-parity-harness.md).
The harness runs against the `scode` binary in a clean environment and
covers the scode-native testable feature surface.

## Case study: first run of the standing rule

The first design write-up under this rule — the `!` bash mode
section inside [`../ROADMAP.md`](../ROADMAP.md) Goal 3 — made the
value concrete:

- Validated that our `Stdio::null()` choice for the bash spawn path is
  functionally equivalent to CC's `pipe` choice for the current scope,
  and recorded what the difference unlocks if we ever want interactive
  passthrough.
- Identified CC's `pwd -P >| <track_file>` pattern as the right
  blueprint for the `!cd` cwd-persistence requirement in
  [ROADMAP Goal 3](../ROADMAP.md).
- Captured six adjacent CC patterns that are out of scope for the
  current cycle but each carry a recorded trigger condition for when
  we should revisit them.

None of these would have surfaced from the CHANGELOG entries alone.
Every design write-up for a feature with parity intent goes through
the same loop.

## Quick reference: where each source enters a parity decision

| Step | Source | Action |
|---|---|---|
| 1. Identify the gap | Tier 1 (CHANGELOG / npm / docs) | List the CHANGELOG entry or surface diff. |
| 2. Align understanding | Tier 2 (CCB grep) | Open CCB, read the actual implementation, confirm the intent. |
| 3. Pick the resolution | All three | Decide `[BUILD]` / `[CHERRY-PICK]` / `[SKIP]` / `[N/A]` / `[OBSERVE]`. |
| 4. Execute | Tier 3 if `[CHERRY-PICK]`, otherwise hand-rolled | Apply the resolution. |
| 5. Anchor | `LAST_PARITY_SYNC_COMMIT` / `LAST_CCB_REF_VERSION` | Update marker files when the cycle closes. |
