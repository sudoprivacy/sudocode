# Sudo Code documentation

Topic-scoped reference docs for `scode`. Each page is the single source
of truth for its topic; other docs link into them rather than restate
the content.

## Using `scode`

- [`usage.md`](./usage.md) — REPL, one-shot prompts, JSON output,
  session resume, `scode doctor`.
- [`authentication.md`](./authentication.md) — subscription / proxy /
  api-key modes, environment variables, endpoints.
- [`permissions-and-sandbox.md`](./permissions-and-sandbox.md) —
  permission modes and the Linux user-namespace sandbox.
- [`acp.md`](./acp.md) — Agent Communication Protocol transports
  (stdio + WebSocket) and the embedded Web UI.
- [`models.md`](./models.md) — model aliases and provider-specific
  request handling.
- [`plugins.md`](./plugins.md) — authoring and using `scode` plugins
  (Chinese: [`plugins_zh.md`](./plugins_zh.md)).
- [`container.md`](./container.md) — building and running `scode`
  inside a container.

## Project mechanics

- Parity mechanism — inlined in [`../ROADMAP.html`](../ROADMAP.html)
  under Goal 2: reference sources (public CC surfaces, the private
  `sudoprivacy/claude-code` snapshot, runtime-observation combos,
  CCB, claw-code), the mandatory "CHANGELOG → grep CCB → align"
  loop, resolution taxonomy, and the sync markers
  (`LAST_PARITY_SYNC_COMMIT`, `LAST_CCB_REF_VERSION`).
- [`mock-parity-harness.md`](./mock-parity-harness.md) — the
  deterministic mock backend and the harness that exercises the parity
  scenarios.

## Plan

- [`../ROADMAP.html`](../ROADMAP.html) — single SSOT plan file. Project
  goals plus the active design detail for each goal (e2e coverage
  inventory under Goal 1, `!` bash mode + TUI enhancement under
  Goal 3, etc.). All plan content lives here.

## See also

- [`../README.md`](../README.md) — project entry, install, quick start.
- [`../ROADMAP.html`](../ROADMAP.html) — project goals.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) — contributor setup and PR
  workflow.
- [`../rust/README.md`](../rust/README.md) — Cargo workspace map.
