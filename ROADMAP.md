# Sudo Code Roadmap

> **Goals only.** This document describes where `sudocode` is going.
> Day-to-day work — tasks, sprint boards, weekly schedules — lives in PRs,
> issues, and 1:1 notes.

## What sudo code is

`sudocode` (binary: `scode`) is a Rust-native ACP engine for coding agents
— the hacker-facing CLI half of the sudo* family.

| | sudowork | sudocode |
|---|---|---|
| Audience | Non-technical end users | Developers, hackers, machines |
| Surface | GUI / Electron | CLI / headless / ACP |
| Defaults | Safe, hand-held, friendly copy | Composable, terse, full power |
| Relationship | sudowork uses sudocode as one of its execution engines | — |

The two are tuned for different audiences. The same capability can land
safely-defaulted in sudowork and exposed as a full knob in sudocode.

## North star

- **Rust-native** — single binary, deterministic shutdown, lean footprint.
- **Model-agnostic** — Anthropic, OpenAI, xAI, Gemini, OAuth subscriptions,
  arbitrary proxy backends.
- **Headless-first** — ACP over stdio and WebSocket; embeddable as
  "agent as a service."
- **Safe by design** — explicit permission modes plus a Linux
  user-namespace sandbox.

## Active goals — 2026-Q2

### Goal 1 · Lock the baseline

`scode` CLI e2e coverage reaches and stays at **≥ 90%** of the
scode-native testable feature surface, green on every `main` commit of
sudocode CI.

Coverage scope and the test surface are described in
[`docs/parity.md`](./docs/parity.md). The mock parity harness used to
exercise this surface is described in
[`docs/mock-parity-harness.md`](./docs/mock-parity-harness.md).

### Goal 2 · claude-code parity

Every feature gap between `scode` and `anthropics/claude-code` carries a
written resolution — `[BUILD]` / `[CHERRY-PICK]` / `[SKIP]` / `[N/A]` /
`[OBSERVE]` — with a one-line rationale.

The reference target is `anthropics/claude-code`. Comparison runs against
public release notes, the published npm bundle's tool and slash
surfaces, and official documentation, anchored to a tracked sync marker.
The mechanism is described in [`docs/parity.md`](./docs/parity.md).

`ultraworkers/claw-code` is a cherry-pick source for Rust-side
implementations of overlapping features. Their work is treated as
optional input rather than upstream-of-truth.

### Goal 3 · Ship features real users miss

When an actual user — internal or external — hits a sharp edge in `scode`
that `claude-code` has already smoothed, the feature lands here as a
committed item.

| Feature | Source signal |
|---|---|
| `!` bash mode (inline shell from prompt) | 武鹏 — 2026-06-10 (内部用户) |

#### Implementation note — `!` bash mode

A prompt beginning with `!` dispatches directly to `runtime::bash` —
`!ls`, `!git status`, `!cd path`, and so on — matching `claude-code`'s
bash-mode semantics for muscle-memory parity.

`scode`'s bash mode additionally:

- Displays the resolved `pwd` on every bash-mode prompt redraw, so the
  active working directory is always visible.
- Threads `cwd` state through `!cd` so subsequent prompt-driven and
  LLM-driven tool calls share the same directory view.
- Routes every `!` command through the same validators as the
  LLM-driven `bash` tool path, so the active permission mode applies
  identically.

## Working agreement on this document

Scope changes update this document in the same PR. External
communications can mirror the current state into other surfaces; this
document remains the canonical reference.

## Pointers

- [`docs/parity.md`](./docs/parity.md) — what parity means for sudocode
  and how we measure it.
- [`docs/mock-parity-harness.md`](./docs/mock-parity-harness.md) — the
  harness that exercises the e2e surface.
- [`docs/plans/`](./docs/plans/) — active and archived design plans.
- [`README.md`](./README.md) — project entry, install, quick start.
