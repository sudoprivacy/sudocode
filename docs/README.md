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

- [`parity.md`](./parity.md) — what claude-code parity means for `scode`
  and how it is tracked.
- [`mock-parity-harness.md`](./mock-parity-harness.md) — the
  deterministic mock backend and the harness that exercises the parity
  scenarios.

## Plans

- [`plans/active/`](./plans/active/) — in-flight design plans.
- [`plans/archive/`](./plans/archive/) — landed and superseded plans.
- [`plans/README.md`](./plans/README.md) — index of both.

## See also

- [`../README.md`](../README.md) — project entry, install, quick start.
- [`../ROADMAP.md`](../ROADMAP.md) — project goals.
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) — contributor setup and PR
  workflow.
- [`../rust/README.md`](../rust/README.md) — Cargo workspace map.
