# Sudo Code Roadmap

> **Living document. Goals only — no task tracking.**
> 这里只写**最终目标**，不追踪每日任务。
> Tactical weekly plans are ephemeral; the roadmap is the single source of truth for
> what sudo code is becoming.

## What sudo code is

`sudocode` (binary: `scode`) is a Rust-native ACP engine for coding agents — the
**hacker-facing CLI half** of the sudo* family.

| | sudowork | sudocode |
|---|---|---|
| Audience | Non-technical end users | Developers, hackers, machines |
| Surface | GUI / Electron | CLI / headless / ACP |
| Defaults | Safe, hand-held, friendly copy | Composable, terse, full power |
| Relationship | sudowork **uses** sudocode as one of its execution engines | — |

The two are deliberately tuned for different audiences. The same capability
can land safely-defaulted in sudowork while exposing the full knob in sudocode.

## North star

- **Rust-native** — single binary, no Node/Python startup tax, deterministic shutdown
- **Model-agnostic** — Anthropic, OpenAI, xAI, Gemini, plus OAuth subscriptions and arbitrary proxy backends
- **Headless-first** — ACP over both stdio and WebSocket; embeddable as "agent as a service"
- **Safe by design** — explicit permission modes (`read-only` / `workspace-write` / `danger-full-access`) + Linux user-namespace sandbox

## Active goals — 2026-Q2

### Goal 1 · Lock the baseline

**e2e coverage ≥ 90% on sudocode CI, green every commit.**

- Coverage is measured over **scode-native testable features**:
  - Drop sudowork-UI-only items (agent switching in UI, `/auth` UI toggle)
  - Drop L2-deferred items (LSP ×4, Workers/Teams/Cron ×3, Plugins lifecycle ×1) — these need heavier setup and land in a follow-up quarter
- Current denominator: ~44 features. 90% = 40 covered.
- Tests live in `rust/crates/rusty-sudocode-cli/tests/`; new cases extend
  `mock_parity_harness.rs` + `mock_parity_scenarios.json`.
- Live API smoke (`acp_live_smoke`) stays gated on main push to defend against
  real API drift.

### Goal 2 · claude-code parity

**Every gap vs `anthropics/claude-code` has a written resolution.**
"Resolution" ≠ "implemented" — a concrete plan also counts.

- Reference target: `anthropics/claude-code` (source code is private; we work
  from the public CHANGELOG, the npm bundle's tool/slash surface, and official docs)
- Sync mechanism: tracked in `rust/PARITY.md`. `LAST_PARITY_SYNC_COMMIT`
  semantics will be replaced with `LAST_CLAUDE_CODE_VERSION` (a release tag).
- Resolution tags per gap: `[BUILD]` / `[CHERRY-PICK]` / `[SKIP]` / `[N/A]` /
  `[OBSERVE]` (watch how `ultraworkers/claw-code` ports it)
- `ultraworkers/claw-code` is kept as a **cherry-pick source**, not as
  upstream-of-truth. They diverged from claude-code parity and now develop
  independently; we still pull what's useful.
- Comparison window starts at `claude-code` releases shipped on or after
  **2026-01-01** (~4 months of feature surface, aligned with our assistant's
  knowledge cutoff).

### Goal 3 · Ship features real users miss

User friction beats CHANGELOG triage. Items here are committed because an
actual sudocode user (internal or external) hit the gap.

| Feature | Source signal | Committed |
|---|---|---|
| `!` bash mode (inline shell from prompt) | 武鹏 2026-06-10 (内部用户) | ✅ |

#### Implementation note — `!` bash mode

What it does: a prompt that starts with `!` bypasses the LLM round-trip and
dispatches directly to `runtime::bash` — `!ls`, `!git status`, `!cd path`,
etc. Matches `claude-code`'s bash-mode semantics for muscle-memory parity.

Where we go further than claude-code:
- **Always show `pwd` in the bash-mode prompt.** Hacker friction point: in
  claude-code, `!ls` doesn't display the working directory, so the user is
  blind to what they're listing. sudocode's bash mode shows the resolved
  `pwd` on every prompt redraw.
- Preserve session `cwd` across `!cd` invocations and surface it back to the
  LLM context so subsequent tool calls inherit the same view.
- Honor the active permission mode (`!rm -rf /` still goes through the same
  `bash` validators as the LLM-driven tool path; no permission shortcut).

## Working agreement on this doc

- Update **here** when scope shifts. PRs that change scope touch ROADMAP.md.
- Daily task tracking, sprint boards, weekly schedules — **not here**. Those
  are ephemeral and live in PRs, issues, or 1:1 notes.
- External communication (周报, 进鲸 / cross-team updates) can mirror this
  doc on ShareOne or similar surfaces. Mirrors are point-in-time snapshots,
  not authoritative.

## Out of scope (2026-Q2)

- Full plugin install/enable/disable/uninstall lifecycle (deferred to Q3)
- LSP integration depth beyond surface parity (deferred)
- Workers / Teams / Cron wall-clock testing (deferred)
- Full claude-code source-level diff (their source is private; we work from
  the public surface)

## Pointers

- `rust/PARITY.md` — detailed parity status (tool surface, slash commands, behavioral checkpoints)
- `rust/crates/rusty-sudocode-cli/tests/` — e2e harness + scenarios
- `docs/plans/` — historical / experimental plan files (archive)
- `README.md` — what sudocode is, how to install, how to run

---

*Last touched: 2026-06-10. This doc is a goal-only living document; if you
need a snapshot for external sharing, mirror the current state to ShareOne
or similar and link back here.*
